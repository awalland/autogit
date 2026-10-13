mod coordinator;
mod git;
mod socket;
mod tray;

use anyhow::{Context, Result};
use autogit_shared::Config;
use coordinator::Coordinator;
use notify::{Event, RecursiveMode, Watcher};
use std::path::Path;
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
    let _watcher = watch_config(&config_path, reload_tx)?;
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
            biased;
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

fn watch_config(path: &Path, reload: mpsc::Sender<()>) -> Result<notify::RecommendedWatcher> {
    let target = path.to_owned();
    let mut watcher =
        notify::recommended_watcher(move |event: Result<Event, notify::Error>| match event {
            Ok(event)
                if event.paths.iter().any(|path| path == &target) && !event.kind.is_access() =>
            {
                // Coalesce events without blocking the notify thread.
                let _ = reload.try_send(());
            }
            Err(error) => error!(%error, "Configuration watch failed"),
            _ => {}
        })?;
    watcher.watch(
        path.parent().context("Configuration path has no parent")?,
        RecursiveMode::NonRecursive,
    )?;
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
    let mut cycles = 0;
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
            let completed = state.completed_cycles != cycles;
            cycles = state.completed_cycles;
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                tray.update(|tray| {
                    tray.update_repo_details(details.clone());
                    tray.set_repo_count(config.repositories.len());
                    tray.set_check_interval(config.daemon.check_interval_seconds);
                    if state.syncing {
                        tray.set_status(tray::TrayStatus::Syncing);
                    } else if details.iter().any(|detail| detail.error.is_some()) {
                        if completed {
                            tray.increment_errors();
                        } else {
                            tray.set_status(tray::TrayStatus::Error);
                        }
                    } else if completed {
                        tray.set_last_sync();
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
