mod coordinator;
mod git;
mod socket;
mod tray;

use anyhow::{Context, Result};
use autogit_shared::{Config, RepoDetail};
use coordinator::Coordinator;
use notify::{Event, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{RwLock, mpsc, watch};
use tokio::task::JoinSet;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    info!("Starting autogit-daemon v{}", env!("CARGO_PKG_VERSION"));
    let config_path = Config::default_config_path()?;
    let mut owner = socket::SocketOwner::acquire(&autogit_shared::socket_path()?)?;
    let config = Config::load_or_create_default().context("Failed to load configuration")?;
    let config = Arc::new(RwLock::new(config));
    // Establish controls and watching before starting any repository work.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let listener = owner.bind()?;
    let (reload_tx, mut reload_rx) = mpsc::channel(1);
    let mut watcher = ConfigWatcher::new(&config_path, reload_tx)?;
    // Cover a replacement between the initial load and establishing the directory watch.
    if let Ok(latest) = Config::load(&config_path) {
        *config.write().await = latest;
    }
    let suspended = Arc::new(AtomicBool::new(false));
    let (coordinator, worker) = Coordinator::start(config.clone(), suspended);
    let (actions_tx, mut actions_rx) = mpsc::channel(16);
    let (tray_stop, tray_stop_rx) = watch::channel(false);
    let tray_worker = tokio::spawn(manage_tray(
        config.clone(),
        coordinator.clone(),
        actions_tx,
        tray_stop_rx,
    ));
    let mut connections = JoinSet::new();
    let started = Instant::now();
    let mut interval = make_interval(config.read().await.daemon.check_interval_seconds);
    coordinator.schedule(None);
    loop {
        tokio::select! {
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            Some(action) = actions_rx.recv() => match action {
                tray::TrayAction::Quit => break,
                tray::TrayAction::TriggerSync => coordinator.schedule(None),
                tray::TrayAction::ToggleSuspend => {
                    let suspended = !coordinator.suspended.load(Ordering::SeqCst);
                    coordinator.set_suspended(suspended);
                    if !suspended { coordinator.schedule(None); }
                }
            },
            result = listener.accept(), if connections.len() < 64 => {
                match result {
                    Ok((stream, _)) => { connections.spawn(socket::handle_connection(stream, config.clone(), started, coordinator.clone())); }
                    Err(error) => warn!(%error, "Socket accept failed"),
                }
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            _ = interval.tick() => coordinator.schedule(None),
            Some(()) = reload_rx.recv() => {
                if let Err(error) = watcher.refresh() {
                    error!(%error, "Configuration watch refresh failed");
                }
                match Config::load(&config_path) {
                    Ok(updated) => {
                        let old = config.read().await.clone();
                        let added = updated.repositories.iter().filter(|repository| !old.repositories.iter().any(|previous| previous.path == repository.path)).map(|repository| repository.path.clone()).collect::<Vec<_>>();
                        if old.daemon.check_interval_seconds != updated.daemon.check_interval_seconds {
                            interval = make_interval(updated.daemon.check_interval_seconds);
                        }
                        *config.write().await = updated;
                        info!("Configuration reloaded");
                        if !added.is_empty() { coordinator.schedule(Some(added)); }
                    }
                    Err(error) => error!(%error, "Configuration reload failed; retaining active configuration"),
                }
            }
        }
    }
    info!("Shutting down daemon");
    coordinator.shutdown();
    // Git cancellation kills and reaps managed subprocesses before releasing the socket lock.
    worker.await.context("Sync coordinator failed")?;
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    let _ = tray_stop.send(true);
    tray_worker.await.context("Tray manager failed")?;
    drop(owner);
    Ok(())
}

fn make_interval(seconds: u64) -> tokio::time::Interval {
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(seconds),
        Duration::from_secs(seconds),
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

struct ConfigWatcher {
    path: PathBuf,
    reload: mpsc::Sender<()>,
    watcher: notify::RecommendedWatcher,
}

impl ConfigWatcher {
    fn new(path: &Path, reload: mpsc::Sender<()>) -> Result<Self> {
        let watcher = watch_config(path, reload.clone())?;
        Ok(Self {
            path: path.to_owned(),
            reload,
            watcher,
        })
    }

    fn refresh(&mut self) -> Result<()> {
        // Install new watches before dropping old ones so link retargets do not leave a gap.
        self.watcher = watch_config(&self.path, self.reload.clone())?;
        Ok(())
    }
}

fn watch_config(path: &Path, reload: mpsc::Sender<()>) -> Result<notify::RecommendedWatcher> {
    // Watch each link and the target, including ancestor replacement or creation.
    let targets = Config::watch_paths(path)?
        .into_iter()
        .flat_map(|path| {
            path.ancestors()
                .filter(|ancestor| ancestor.parent().is_some())
                .map(Path::to_owned)
                .collect::<Vec<_>>()
        })
        .collect::<HashSet<_>>();
    let directories = targets
        .iter()
        .filter_map(|target| target.parent())
        .filter(|directory| directory.is_dir())
        .map(Path::to_owned)
        .collect::<HashSet<_>>();
    let mut watcher =
        notify::recommended_watcher(move |event: Result<Event, notify::Error>| match event {
            Ok(event)
                if !event.kind.is_access()
                    && event.paths.iter().any(|path| targets.contains(path)) =>
            {
                let _ = reload.try_send(());
            }
            Err(error) => error!(%error, "Configuration watch failed"),
            _ => {}
        })?;
    for directory in directories {
        watcher.watch(&directory, RecursiveMode::NonRecursive)?;
    }
    Ok(watcher)
}

async fn shutdown_tray(handle: ksni::Handle<tray::AutogitTray>) {
    // shutdown() sends the stop signal immediately; awaiting it only observes service exit.
    if tokio::time::timeout(Duration::from_secs(2), handle.shutdown())
        .await
        .is_err()
    {
        warn!("Timed out waiting for tray shutdown");
    }
}

async fn manage_tray(
    config: Arc<RwLock<Config>>,
    coordinator: Coordinator,
    actions: mpsc::Sender<tray::TrayAction>,
    mut stop: watch::Receiver<bool>,
) {
    let mut handle: Option<ksni::Handle<tray::AutogitTray>> = None;
    let mut retries = 0;
    let mut next_retry = Instant::now();
    loop {
        tokio::select! {
            _ = stop.changed() => { if *stop.borrow() { break; } }
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
        let config = config.read().await.clone();
        if !config.daemon.enable_tray {
            if let Some(old) = handle.take() {
                shutdown_tray(old).await;
            }
            retries = 0;
            next_retry = Instant::now();
            continue;
        }
        if handle.is_none() && retries < 3 && Instant::now() >= next_retry {
            retries += 1;
            next_retry = Instant::now() + Duration::from_secs(30);
            let tray = tray::AutogitTray::new(
                config.repositories.len(),
                actions.clone(),
                coordinator.suspended.clone(),
            );
            match tokio::time::timeout(Duration::from_secs(5), tray.spawn_tray()).await {
                Ok(Ok(created)) => handle = Some(created),
                Ok(Err(error)) => warn!(%error, "Tray initialization failed"),
                Err(_) => warn!("Tray initialization timed out"),
            }
        }
        if let Some(tray) = &handle {
            let state = coordinator.state.borrow().clone();
            let details = state
                .details
                .iter()
                .filter(|detail| {
                    config
                        .repositories
                        .iter()
                        .any(|repo| repo.path == detail.path)
                })
                .cloned()
                .collect::<Vec<_>>();
            let error_count = active_error_count(&config, &details);
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                tray.update(|tray| {
                    tray.update_repo_details(details.clone());
                    tray.set_repo_count(config.repositories.len());
                    tray.set_check_interval(config.daemon.check_interval_seconds);
                    tray.set_error_count(error_count);
                    tray.set_last_sync(state.last_successful_sync);
                    if state.syncing {
                        tray.set_status(tray::TrayStatus::Syncing);
                    } else if error_count > 0 {
                        tray.set_status(tray::TrayStatus::Error);
                    } else {
                        tray.set_status(tray::TrayStatus::Idle);
                    }
                }),
            )
            .await;
        }
    }
    if let Some(handle) = handle {
        shutdown_tray(handle).await;
    }
}

fn active_error_count(config: &Config, details: &[RepoDetail]) -> usize {
    details
        .iter()
        .filter(|detail| {
            detail.error.is_some()
                && config
                    .repositories
                    .iter()
                    .any(|repository| repository.auto_commit && repository.path == detail.path)
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use autogit_shared::Repository;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn disabled_repositories_do_not_keep_the_tray_in_error() {
        let path = PathBuf::from("/test/repository");
        let mut config = Config::default();
        config.repositories.push(Repository {
            path: path.clone(),
            auto_commit: true,
            commit_message_template: "Test commit".to_owned(),
        });
        let details = vec![RepoDetail {
            path,
            committed: false,
            files_changed: None,
            error: Some("Failed to pull".to_owned()),
        }];
        assert_eq!(active_error_count(&config, &details), 1);
        config.repositories[0].auto_commit = false;
        assert_eq!(active_error_count(&config, &details), 0);
        config.repositories.clear();
        assert_eq!(active_error_count(&config, &details), 0);
    }

    #[tokio::test]
    async fn tray_service_shutdown_closes_handle_before_replacement() {
        if std::env::var_os("AUTOGIT_TRAY_LIFECYCLE_CHILD").is_none() {
            let child = tokio::process::Command::new("dbus-run-session")
                .arg("--")
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::tray_service_shutdown_closes_handle_before_replacement",
                    "--nocapture",
                ])
                .env("AUTOGIT_TRAY_LIFECYCLE_CHILD", "1")
                .kill_on_drop(true)
                .output();
            let output = tokio::time::timeout(Duration::from_secs(10), child)
                .await
                .unwrap()
                .expect("dbus-run-session is required for tray lifecycle tests");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use ksni::TrayMethods;
        let (actions, _) = mpsc::channel(1);
        let suspended = Arc::new(AtomicBool::new(false));
        for _ in 0..2 {
            let tray = tray::AutogitTray::new(0, actions.clone(), suspended.clone());
            let handle = tray.assume_sni_available(true).spawn().await.unwrap();
            assert!(!handle.is_closed());
            let witness = handle.clone();
            shutdown_tray(handle).await;
            assert!(witness.is_closed());
        }
    }

    #[tokio::test]
    async fn watcher_follows_multihop_links_and_retargets() {
        use std::os::unix::fs::symlink;
        let directory = TempDir::new().unwrap();
        let alias = directory.path().join("alias");
        let middle = directory.path().join("middle");
        let first_dir = directory.path().join("first");
        let second_dir = directory.path().join("second");
        std::fs::create_dir(&first_dir).unwrap();
        std::fs::create_dir(&second_dir).unwrap();
        let first = first_dir.join("config.toml");
        let second = second_dir.join("config.toml");
        std::fs::write(&first, "initial").unwrap();
        std::fs::write(&second, "second").unwrap();
        symlink(&middle, &alias).unwrap();
        symlink(&first, &middle).unwrap();
        let (sender, mut receiver) = mpsc::channel(1);
        let mut watcher = ConfigWatcher::new(&alias, sender).unwrap();
        let replacement = first_dir.join("temporary");
        std::fs::write(&replacement, "replacement").unwrap();
        std::fs::rename(&replacement, &first).unwrap();
        tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        watcher.refresh().unwrap();
        let new_link = directory.path().join("new_link");
        symlink(&second, &new_link).unwrap();
        std::fs::rename(&new_link, &middle).unwrap();
        tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        watcher.refresh().unwrap();
        // Drain already coalesced replacement events before checking the new target.
        while receiver.try_recv().is_ok() {}
        std::fs::write(&second, "updated").unwrap();
        tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read_to_string(&alias).unwrap(), "updated");
    }

    #[tokio::test]
    async fn watcher_survives_repeated_atomic_replacements() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "initial").unwrap();
        let (sender, mut receiver) = mpsc::channel(1);
        let _watcher = watch_config(&path, sender).unwrap();
        for content in ["first", "second"] {
            let replacement = directory.path().join("temporary");
            std::fs::write(&replacement, content).unwrap();
            std::fs::rename(replacement, &path).unwrap();
            tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }
    }
}
