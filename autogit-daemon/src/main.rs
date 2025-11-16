mod git;
mod socket;
mod tray;

use anyhow::{Context, Result};
use autogit_shared::Config;
use notify::{Watcher, RecursiveMode, Event};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{RwLock, mpsc};
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, error, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        )
        .init();

    info!("Starting autogit-daemon v{}", env!("CARGO_PKG_VERSION"));

    // Load configuration
    let config_path = Config::default_config_path()
        .context("Failed to get default config path")?;

    info!("Loading configuration from: {}", config_path.display());

    let config = Config::load_or_create_default()
        .context("Failed to load configuration")?;

    info!("Loaded configuration with {} repositories", config.repositories.len());

    // Initialize all repositories (commit pending changes and pull)
    info!("Initializing repositories...");
    for repo in &config.repositories {
        if !repo.auto_commit {
            continue;
        }

        match git::initialize_repository(repo).await {
            Ok(()) => {
                info!("Initialized repository: {}", repo.path.display());
            }
            Err(e) => {
                error!("Error initializing repository {}: {:#}", repo.path.display(), e);
            }
        }
    }
    info!("Repository initialization complete");

    // Wrap config in Arc<RwLock> so we can reload it
    let config = Arc::new(RwLock::new(config));

    // Set up config file watcher
    let (reload_tx, reload_rx) = mpsc::channel(10);
    let config_path_clone = config_path.clone();

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        match res {
            Ok(event) => {
                // Only trigger reload on modify events
                if event.kind.is_modify() {
                    info!("Config file changed, triggering reload");
                    let _ = reload_tx.blocking_send(());
                }
            }
            Err(e) => {
                error!("Config file watch error: {:?}", e);
            }
        }
    })
    .context("Failed to create config file watcher")?;

    // Watch the config file
    watcher.watch(&config_path, RecursiveMode::NonRecursive)
        .with_context(|| format!("Failed to watch config file: {}", config_path.display()))?;

    info!("Watching config file for changes: {}", config_path.display());

    // Set up Unix socket for CLI communication
    let socket_listener = socket::create_listener()
        .context("Failed to create Unix socket listener")?;

    // Set up system tray icon with suspended state (optional - won't fail if no tray available)
    let suspended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tray_action_tx, tray_action_rx) = mpsc::channel(10);

    // Check if tray is enabled in config
    let enable_tray = config.read().await.daemon.enable_tray;
    let initial_tray = if enable_tray {
        let repo_count = config.read().await.repositories.len();
        let tray = tray::AutogitTray::new(repo_count, tray_action_tx.clone(), suspended.clone());
        match tray.spawn_tray().await {
            Ok(handle) => {
                info!("System tray icon spawned successfully");
                Some(handle)
            }
            Err(e) => {
                warn!("Failed to spawn system tray icon (no desktop environment?): {:#}", e);
                warn!("Daemon will continue without tray icon");
                None
            }
        }
    } else {
        info!("System tray disabled in configuration");
        None
    };

    let tray_handle = Arc::new(RwLock::new(initial_tray));

    // Set up signal handling for graceful shutdown
    let sigterm = signal(SignalKind::terminate())
        .context("Failed to create SIGTERM handler")?;
    let sigint = signal(SignalKind::interrupt())
        .context("Failed to create SIGINT handler")?;

    // Track daemon start time for uptime reporting
    let start_time = Instant::now();

    // Start the main daemon loop
    run_daemon(
        config,
        config_path_clone,
        reload_rx,
        sigterm,
        sigint,
        socket_listener,
        start_time,
        tray_handle,
        tray_action_rx,
        suspended,
        tray_action_tx,
    ).await?;

    // Keep watcher alive until daemon exits
    drop(watcher);

    // Clean up socket file
    socket::cleanup_socket();

    info!("autogit-daemon shutting down");
    Ok(())
}

pub(crate) async fn run_daemon(
    config: Arc<RwLock<Config>>,
    config_path: PathBuf,
    mut reload_rx: mpsc::Receiver<()>,
    mut sigterm: tokio::signal::unix::Signal,
    mut sigint: tokio::signal::unix::Signal,
    socket_listener: tokio::net::UnixListener,
    start_time: Instant,
    tray_handle: Arc<RwLock<Option<ksni::Handle<tray::AutogitTray>>>>,
    mut tray_action_rx: mpsc::Receiver<tray::TrayAction>,
    suspended: Arc<std::sync::atomic::AtomicBool>,
    tray_action_tx: mpsc::Sender<tray::TrayAction>,
) -> Result<()> {
    let mut interval = {
        let cfg = config.read().await;
        tokio::time::interval(std::time::Duration::from_secs(
            cfg.daemon.check_interval_seconds
        ))
    };

    // Skip the first immediate tick since we already initialized repositories
    interval.tick().await;

    loop {
        tokio::select! {
            biased;

            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down gracefully");
                break;
            }

            _ = sigint.recv() => {
                info!("Received SIGINT (Ctrl+C), shutting down gracefully");
                break;
            }

            // Handle incoming socket connections
            Ok((stream, _addr)) = socket_listener.accept() => {
                let config_clone = Arc::clone(&config);
                let suspended_clone = Arc::clone(&suspended);
                tokio::spawn(async move {
                    socket::handle_connection(stream, config_clone, start_time, suspended_clone).await;
                });
            }

            _ = interval.tick() => {
                // Skip if daemon is suspended
                if suspended.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }

                // Normal check cycle
                if let Some(ref tray) = tray_handle.read().await.as_ref() {
                    tray.update(|t| {
                        let _ = t.set_status(tray::TrayStatus::Syncing);
                    }).await;
                }

                let cfg = config.read().await;
                let mut any_errors = false;

                // Process each repository
                for repo in &cfg.repositories {
                    if !repo.auto_commit {
                        continue;
                    }

                    match git::check_and_commit(repo).await {
                        Ok(committed) => {
                            if committed {
                                info!("Committed changes in: {}", repo.path.display());
                            }
                        }
                        Err(e) => {
                            error!("Error processing repository {}: {:#}", repo.path.display(), e);
                            any_errors = true;
                        }
                    }
                }

                // Update tray status
                if let Some(ref tray) = tray_handle.read().await.as_ref() {
                    if any_errors {
                        tray.update(|t| {
                            let _ = t.increment_errors();
                        }).await;
                    } else {
                        tray.update(|t| {
                            let _ = t.set_last_sync();
                        }).await;
                    }
                }
            }

            Some(_) = reload_rx.recv() => {
                // Config file changed, reload it
                info!("Reloading configuration from: {}", config_path.display());

                match Config::load(&config_path) {
                    Ok(new_config) => {
                        let (old_interval, old_enable_tray) = {
                            let cfg = config.read().await;
                            (cfg.daemon.check_interval_seconds, cfg.daemon.enable_tray)
                        };

                        let new_interval = new_config.daemon.check_interval_seconds;
                        let new_enable_tray = new_config.daemon.enable_tray;

                        // Find new repositories (those not in old config)
                        let new_repos: Vec<_> = {
                            let old_config = config.read().await;
                            new_config.repositories.iter()
                                .filter(|new_repo| {
                                    !old_config.repositories.iter()
                                        .any(|old_repo| old_repo.path == new_repo.path)
                                })
                                .cloned()
                                .collect()
                        };

                        // Initialize new repositories
                        for repo in &new_repos {
                            if !repo.auto_commit {
                                continue;
                            }

                            info!("Initializing newly added repository: {}", repo.path.display());
                            match git::initialize_repository(repo).await {
                                Ok(()) => {
                                    info!("Initialized repository: {}", repo.path.display());
                                }
                                Err(e) => {
                                    error!("Error initializing repository {}: {:#}", repo.path.display(), e);
                                }
                            }
                        }

                        // Update config
                        *config.write().await = new_config;

                        let new_repo_count = config.read().await.repositories.len();
                        info!("Configuration reloaded successfully with {} repositories", new_repo_count);

                        // Update tray with new repository count
                        if let Some(ref tray) = tray_handle.read().await.as_ref() {
                            tray.update(|t| {
                                let _ = t.set_repo_count(new_repo_count);
                            }).await;
                        }

                        // Update interval if it changed
                        if old_interval != new_interval {
                            info!("Check interval changed from {}s to {}s, updating timer",
                                  old_interval, new_interval);
                            interval = tokio::time::interval(std::time::Duration::from_secs(new_interval));
                        }

                        // Handle tray enable/disable changes
                        if old_enable_tray != new_enable_tray {
                            if new_enable_tray {
                                // Tray was disabled, now enable it
                                info!("System tray enabled in configuration, spawning tray icon");
                                let repo_count = new_repo_count;
                                let tray = tray::AutogitTray::new(repo_count, tray_action_tx.clone(), suspended.clone());
                                match tray.spawn_tray().await {
                                    Ok(handle) => {
                                        info!("System tray icon spawned successfully");
                                        *tray_handle.write().await = Some(handle);
                                    }
                                    Err(e) => {
                                        warn!("Failed to spawn system tray icon: {:#}", e);
                                        warn!("Tray will remain disabled");
                                    }
                                }
                            } else {
                                // Tray was enabled, now disable it
                                info!("System tray disabled in configuration, removing tray icon");
                                *tray_handle.write().await = None;
                                info!("System tray icon removed");
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to reload configuration: {:#}", e);
                        warn!("Keeping previous configuration");
                    }
                }
            }

            // Handle tray icon actions
            Some(action) = tray_action_rx.recv() => {
                match action {
                    tray::TrayAction::TriggerSync => {
                        // Skip if suspended
                        if suspended.load(std::sync::atomic::Ordering::Relaxed) {
                            info!("Manual sync skipped (daemon is suspended)");
                            continue;
                        }

                        info!("Manual sync triggered from tray icon");
                        if let Some(ref tray) = tray_handle.read().await.as_ref() {
                            tray.update(|t| {
                                let _ = t.set_status(tray::TrayStatus::Syncing);
                            }).await;
                        }

                        let cfg = config.read().await;
                        let mut any_errors = false;

                        for repo in &cfg.repositories {
                            if !repo.auto_commit {
                                continue;
                            }

                            match git::check_and_commit(repo).await {
                                Ok(committed) => {
                                    if committed {
                                        info!("Committed changes in: {}", repo.path.display());
                                    }
                                }
                                Err(e) => {
                                    error!("Error processing repository {}: {:#}", repo.path.display(), e);
                                    any_errors = true;
                                }
                            }
                        }

                        if let Some(ref tray) = tray_handle.read().await.as_ref() {
                            if any_errors {
                                tray.update(|t| {
                                    let _ = t.increment_errors();
                                }).await;
                            } else {
                                tray.update(|t| {
                                    let _ = t.set_last_sync();
                                }).await;
                            }
                        }
                    }

                    tray::TrayAction::ToggleSuspend => {
                        let new_state = !suspended.load(std::sync::atomic::Ordering::Relaxed);
                        suspended.store(new_state, std::sync::atomic::Ordering::Relaxed);

                        if new_state {
                            info!("Daemon suspended");
                        } else {
                            info!("Daemon resumed");
                        }

                        // Update tray to reflect new state
                        if let Some(ref tray) = tray_handle.read().await.as_ref() {
                            tray.update(|_t| {}).await;
                        }
                    }

                    tray::TrayAction::Quit => {
                        info!("Quit requested from tray icon");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use autogit_shared::{Config, DaemonConfig, Repository};
    use std::env;
    use tempfile::TempDir;
    use tokio::net::UnixListener;
    use serial_test::serial;

    // Helper to create a test config
    fn create_test_config() -> Config {
        Config {
            daemon: DaemonConfig {
                check_interval_seconds: 1, // Short interval for testing
                enable_tray: false, // Disable tray in tests
            },
            repositories: vec![],
        }
    }

    // Helper to create a test socket listener
    async fn create_test_socket() -> UnixListener {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");
        UnixListener::bind(&socket_path).unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn test_config_reload_updates_interval() {
        use tokio::signal::unix::{signal, SignalKind};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Create initial config
        let config_path = Config::default_config_path().unwrap();
        let mut config = create_test_config();
        config.daemon.check_interval_seconds = 60;
        config.save(&config_path).unwrap();

        let config = Arc::new(RwLock::new(config));

        // Create channels and signals
        let (reload_tx, reload_rx) = mpsc::channel(10);

        let sigterm = signal(SignalKind::terminate()).unwrap();
        let sigint = signal(SignalKind::interrupt()).unwrap();
        let socket_listener = create_test_socket().await;
        let start_time = Instant::now();

        let config_clone = Arc::clone(&config);
        let config_path_clone = config_path.clone();

        // Create tray-related test parameters
        let tray_handle = Arc::new(RwLock::new(None));
        let (_tray_action_tx, tray_action_rx) = mpsc::channel(10);
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tray_action_tx, _tray_action_rx2) = mpsc::channel(10);

        // Spawn daemon in background
        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(
                config_clone,
                config_path_clone,
                reload_rx,
                sigterm,
                sigint,
                socket_listener,
                start_time,
                tray_handle,
                tray_action_rx,
                suspended,
                tray_action_tx,
            ).await;
        });

        // Give daemon time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify initial interval
        {
            let cfg = config.read().await;
            assert_eq!(cfg.daemon.check_interval_seconds, 60);
        }

        // Update config file with new interval
        let mut new_config = create_test_config();
        new_config.daemon.check_interval_seconds = 120;
        new_config.save(&config_path).unwrap();

        // Trigger reload
        reload_tx.send(()).await.unwrap();

        // Give daemon time to reload
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify interval was updated
        {
            let cfg = config.read().await;
            assert_eq!(cfg.daemon.check_interval_seconds, 120);
        }

        // Shutdown daemon
        daemon_handle.abort();

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
    }

    #[tokio::test]
    #[serial]
    async fn test_config_reload_adds_repositories() {
        use tokio::signal::unix::{signal, SignalKind};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Create initial config with no repos
        let config_path = Config::default_config_path().unwrap();
        let config = create_test_config();
        config.save(&config_path).unwrap();

        let config = Arc::new(RwLock::new(config));

        // Create channels and signals
        let (reload_tx, reload_rx) = mpsc::channel(10);
        let sigterm = signal(SignalKind::terminate()).unwrap();
        let sigint = signal(SignalKind::interrupt()).unwrap();
        let socket_listener = create_test_socket().await;
        let start_time = Instant::now();

        let config_clone = Arc::clone(&config);
        let config_path_clone = config_path.clone();

        // Create tray-related test parameters
        let tray_handle = Arc::new(RwLock::new(None));
        let (_tray_action_tx, tray_action_rx) = mpsc::channel(10);
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tray_action_tx, _tray_action_rx2) = mpsc::channel(10);

        // Spawn daemon in background
        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(
                config_clone,
                config_path_clone,
                reload_rx,
                sigterm,
                sigint,
                socket_listener,
                start_time,
                tray_handle,
                tray_action_rx,
                suspended,
                tray_action_tx,
            ).await;
        });

        // Give daemon time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify no repos initially
        {
            let cfg = config.read().await;
            assert_eq!(cfg.repositories.len(), 0);
        }

        // Update config file with new repositories
        let mut new_config = create_test_config();
        new_config.repositories.push(Repository {
            path: PathBuf::from("/test/repo1"),
            auto_commit: true,
            commit_message_template: "Test: {timestamp}".to_owned(),
        });
        new_config.repositories.push(Repository {
            path: PathBuf::from("/test/repo2"),
            auto_commit: false,
            commit_message_template: "Test2".to_owned(),
        });
        new_config.save(&config_path).unwrap();

        // Trigger reload
        reload_tx.send(()).await.unwrap();

        // Give daemon time to reload
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify repos were added
        {
            let cfg = config.read().await;
            assert_eq!(cfg.repositories.len(), 2);
            assert_eq!(cfg.repositories[0].path, PathBuf::from("/test/repo1"));
            assert_eq!(cfg.repositories[1].path, PathBuf::from("/test/repo2"));
        }

        // Shutdown daemon
        daemon_handle.abort();

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
    }

    #[tokio::test]
    #[serial]
    async fn test_config_reload_handles_invalid_config() {
        use tokio::signal::unix::{signal, SignalKind};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Create initial config
        let config_path = Config::default_config_path().unwrap();
        let mut initial_config = create_test_config();
        initial_config.daemon.check_interval_seconds = 60;
        initial_config.save(&config_path).unwrap();

        let config = Arc::new(RwLock::new(initial_config));

        // Create channels and signals
        let (reload_tx, reload_rx) = mpsc::channel(10);
        let sigterm = signal(SignalKind::terminate()).unwrap();
        let sigint = signal(SignalKind::interrupt()).unwrap();
        let socket_listener = create_test_socket().await;
        let start_time = Instant::now();

        let config_clone = Arc::clone(&config);
        let config_path_clone = config_path.clone();

        // Create tray-related test parameters
        let tray_handle = Arc::new(RwLock::new(None));
        let (_tray_action_tx, tray_action_rx) = mpsc::channel(10);
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tray_action_tx, _tray_action_rx2) = mpsc::channel(10);

        // Spawn daemon in background
        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(
                config_clone,
                config_path_clone,
                reload_rx,
                sigterm,
                sigint,
                socket_listener,
                start_time,
                tray_handle,
                tray_action_rx,
                suspended,
                tray_action_tx,
            ).await;
        });

        // Give daemon time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Write invalid TOML to config file
        std::fs::write(&config_path, "this is not valid toml!!!").unwrap();

        // Trigger reload
        reload_tx.send(()).await.unwrap();

        // Give daemon time to attempt reload
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify original config is still in place (reload failed gracefully)
        {
            let cfg = config.read().await;
            assert_eq!(cfg.daemon.check_interval_seconds, 60);
        }

        // Shutdown daemon
        daemon_handle.abort();

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
    }

    #[tokio::test]
    #[serial]
    async fn test_config_reload_only_initializes_new_repos() {
        use tokio::signal::unix::{signal, SignalKind};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Create initial config with one repo
        let config_path = Config::default_config_path().unwrap();
        let mut initial_config = create_test_config();
        initial_config.repositories.push(Repository {
            path: PathBuf::from("/test/existing"),
            auto_commit: true,
            commit_message_template: "Existing".to_owned(),
        });
        initial_config.save(&config_path).unwrap();

        let config = Arc::new(RwLock::new(initial_config));

        // Create channels and signals
        let (reload_tx, reload_rx) = mpsc::channel(10);
        let sigterm = signal(SignalKind::terminate()).unwrap();
        let sigint = signal(SignalKind::interrupt()).unwrap();
        let socket_listener = create_test_socket().await;
        let start_time = Instant::now();

        let config_clone = Arc::clone(&config);
        let config_path_clone = config_path.clone();

        // Create tray-related test parameters
        let tray_handle = Arc::new(RwLock::new(None));
        let (_tray_action_tx, tray_action_rx) = mpsc::channel(10);
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tray_action_tx, _tray_action_rx2) = mpsc::channel(10);

        // Spawn daemon in background
        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(
                config_clone,
                config_path_clone,
                reload_rx,
                sigterm,
                sigint,
                socket_listener,
                start_time,
                tray_handle,
                tray_action_rx,
                suspended,
                tray_action_tx,
            ).await;
        });

        // Give daemon time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Add a new repository to config
        let mut new_config = create_test_config();
        new_config.repositories.push(Repository {
            path: PathBuf::from("/test/existing"),
            auto_commit: true,
            commit_message_template: "Existing".to_owned(),
        });
        new_config.repositories.push(Repository {
            path: PathBuf::from("/test/new"),
            auto_commit: true,
            commit_message_template: "New".to_owned(),
        });
        new_config.save(&config_path).unwrap();

        // Trigger reload
        reload_tx.send(()).await.unwrap();

        // Give daemon time to reload
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify both repos are present
        {
            let cfg = config.read().await;
            assert_eq!(cfg.repositories.len(), 2);
            assert_eq!(cfg.repositories[0].path, PathBuf::from("/test/existing"));
            assert_eq!(cfg.repositories[1].path, PathBuf::from("/test/new"));
        }

        // Shutdown daemon
        daemon_handle.abort();

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
    }

    #[tokio::test]
    #[serial]
    async fn test_daemon_respects_short_check_interval() {
        use tokio::signal::unix::{signal, SignalKind};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Create config with very short interval
        let config_path = Config::default_config_path().unwrap();
        let mut config = create_test_config();
        config.daemon.check_interval_seconds = 1; // 1 second
        config.save(&config_path).unwrap();

        let config = Arc::new(RwLock::new(config));

        // Create channels and signals
        let (_reload_tx, reload_rx) = mpsc::channel(10);
        let sigterm = signal(SignalKind::terminate()).unwrap();
        let sigint = signal(SignalKind::interrupt()).unwrap();
        let socket_listener = create_test_socket().await;
        let start_time = Instant::now();

        let config_clone = Arc::clone(&config);
        let config_path_clone = config_path.clone();

        // Create tray-related test parameters
        let tray_handle = Arc::new(RwLock::new(None));
        let (_tray_action_tx, tray_action_rx) = mpsc::channel(10);
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tray_action_tx, _tray_action_rx2) = mpsc::channel(10);

        // Spawn daemon in background
        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(
                config_clone,
                config_path_clone,
                reload_rx,
                sigterm,
                sigint,
                socket_listener,
                start_time,
                tray_handle,
                tray_action_rx,
                suspended,
                tray_action_tx,
            ).await;
        });

        // Let it run for a bit
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Daemon should still be running
        assert!(!daemon_handle.is_finished());

        // Shutdown daemon
        daemon_handle.abort();

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
    }

    #[tokio::test]
    #[serial]
    async fn test_config_reload_skips_disabled_repos() {
        use tokio::signal::unix::{signal, SignalKind};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Create initial config
        let config_path = Config::default_config_path().unwrap();
        let config = create_test_config();
        config.save(&config_path).unwrap();

        let config = Arc::new(RwLock::new(config));

        // Create channels and signals
        let (reload_tx, reload_rx) = mpsc::channel(10);
        let sigterm = signal(SignalKind::terminate()).unwrap();
        let sigint = signal(SignalKind::interrupt()).unwrap();
        let socket_listener = create_test_socket().await;
        let start_time = Instant::now();

        let config_clone = Arc::clone(&config);
        let config_path_clone = config_path.clone();

        // Create tray-related test parameters
        let tray_handle = Arc::new(RwLock::new(None));
        let (_tray_action_tx, tray_action_rx) = mpsc::channel(10);
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tray_action_tx, _tray_action_rx2) = mpsc::channel(10);

        // Spawn daemon in background
        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(
                config_clone,
                config_path_clone,
                reload_rx,
                sigterm,
                sigint,
                socket_listener,
                start_time,
                tray_handle,
                tray_action_rx,
                suspended,
                tray_action_tx,
            ).await;
        });

        // Give daemon time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Update config with disabled repo
        let mut new_config = create_test_config();
        new_config.repositories.push(Repository {
            path: PathBuf::from("/test/disabled"),
            auto_commit: false, // Disabled
            commit_message_template: "Disabled".to_owned(),
        });
        new_config.save(&config_path).unwrap();

        // Trigger reload
        reload_tx.send(()).await.unwrap();

        // Give daemon time to reload (should skip initialization of disabled repo)
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify repo is in config but wasn't initialized (we can't directly test
        // initialization here, but the code path is exercised)
        {
            let cfg = config.read().await;
            assert_eq!(cfg.repositories.len(), 1);
            assert_eq!(cfg.repositories[0].auto_commit, false);
        }

        // Shutdown daemon
        daemon_handle.abort();

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
    }

    #[tokio::test]
    #[serial]
    async fn test_tray_action_trigger_sync() {
        use tokio::signal::unix::{signal, SignalKind};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        let config_path = Config::default_config_path().unwrap();
        let config = create_test_config();
        config.save(&config_path).unwrap();

        let config = Arc::new(RwLock::new(config));

        let (_reload_tx, reload_rx) = mpsc::channel(10);
        let sigterm = signal(SignalKind::terminate()).unwrap();
        let sigint = signal(SignalKind::interrupt()).unwrap();
        let socket_listener = create_test_socket().await;
        let start_time = Instant::now();

        let config_clone = Arc::clone(&config);
        let config_path_clone = config_path.clone();

        let tray_handle = Arc::new(RwLock::new(None));
        let (tray_action_tx, tray_action_rx) = mpsc::channel(10);
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tray_action_tx_clone = tray_action_tx.clone();

        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(
                config_clone,
                config_path_clone,
                reload_rx,
                sigterm,
                sigint,
                socket_listener,
                start_time,
                tray_handle,
                tray_action_rx,
                suspended,
                tray_action_tx_clone,
            ).await;
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Send TriggerSync action
        tray_action_tx.send(tray::TrayAction::TriggerSync).await.unwrap();

        // Give time for action to be processed
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Daemon should still be running
        assert!(!daemon_handle.is_finished());

        daemon_handle.abort();
        let _ = std::fs::remove_file(&config_path);
    }

    #[tokio::test]
    #[serial]
    async fn test_tray_action_toggle_suspend() {
        use tokio::signal::unix::{signal, SignalKind};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        let config_path = Config::default_config_path().unwrap();
        let config = create_test_config();
        config.save(&config_path).unwrap();

        let config = Arc::new(RwLock::new(config));

        let (_reload_tx, reload_rx) = mpsc::channel(10);
        let sigterm = signal(SignalKind::terminate()).unwrap();
        let sigint = signal(SignalKind::interrupt()).unwrap();
        let socket_listener = create_test_socket().await;
        let start_time = Instant::now();

        let config_clone = Arc::clone(&config);
        let config_path_clone = config_path.clone();

        let tray_handle = Arc::new(RwLock::new(None));
        let (tray_action_tx, tray_action_rx) = mpsc::channel(10);
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let suspended_clone = Arc::clone(&suspended);
        let tray_action_tx_clone = tray_action_tx.clone();

        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(
                config_clone,
                config_path_clone,
                reload_rx,
                sigterm,
                sigint,
                socket_listener,
                start_time,
                tray_handle,
                tray_action_rx,
                suspended,
                tray_action_tx_clone,
            ).await;
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Initially not suspended
        assert!(!suspended_clone.load(std::sync::atomic::Ordering::Relaxed));

        // Send ToggleSuspend action
        tray_action_tx.send(tray::TrayAction::ToggleSuspend).await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Now should be suspended
        assert!(suspended_clone.load(std::sync::atomic::Ordering::Relaxed));

        daemon_handle.abort();
        let _ = std::fs::remove_file(&config_path);
    }

    #[tokio::test]
    #[serial]
    async fn test_tray_action_quit() {
        use tokio::signal::unix::{signal, SignalKind};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        let config_path = Config::default_config_path().unwrap();
        let config = create_test_config();
        config.save(&config_path).unwrap();

        let config = Arc::new(RwLock::new(config));

        let (_reload_tx, reload_rx) = mpsc::channel(10);
        let sigterm = signal(SignalKind::terminate()).unwrap();
        let sigint = signal(SignalKind::interrupt()).unwrap();
        let socket_listener = create_test_socket().await;
        let start_time = Instant::now();

        let config_clone = Arc::clone(&config);
        let config_path_clone = config_path.clone();

        let tray_handle = Arc::new(RwLock::new(None));
        let (tray_action_tx, tray_action_rx) = mpsc::channel(10);
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tray_action_tx_clone = tray_action_tx.clone();

        let daemon_handle = tokio::spawn(async move {
            run_daemon(
                config_clone,
                config_path_clone,
                reload_rx,
                sigterm,
                sigint,
                socket_listener,
                start_time,
                tray_handle,
                tray_action_rx,
                suspended,
                tray_action_tx_clone,
            ).await
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Send Quit action
        tray_action_tx.send(tray::TrayAction::Quit).await.unwrap();

        // Give time for graceful shutdown
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Daemon should have finished
        assert!(daemon_handle.is_finished());

        let _ = std::fs::remove_file(&config_path);
    }

    #[tokio::test]
    #[serial]
    async fn test_trigger_sync_when_suspended() {
        use tokio::signal::unix::{signal, SignalKind};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        let config_path = Config::default_config_path().unwrap();
        let config = create_test_config();
        config.save(&config_path).unwrap();

        let config = Arc::new(RwLock::new(config));

        let (_reload_tx, reload_rx) = mpsc::channel(10);
        let sigterm = signal(SignalKind::terminate()).unwrap();
        let sigint = signal(SignalKind::interrupt()).unwrap();
        let socket_listener = create_test_socket().await;
        let start_time = Instant::now();

        let config_clone = Arc::clone(&config);
        let config_path_clone = config_path.clone();

        let tray_handle = Arc::new(RwLock::new(None));
        let (tray_action_tx, tray_action_rx) = mpsc::channel(10);
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(true)); // Start suspended
        let tray_action_tx_clone = tray_action_tx.clone();

        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(
                config_clone,
                config_path_clone,
                reload_rx,
                sigterm,
                sigint,
                socket_listener,
                start_time,
                tray_handle,
                tray_action_rx,
                suspended,
                tray_action_tx_clone,
            ).await;
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Try to trigger sync while suspended (should be skipped)
        tray_action_tx.send(tray::TrayAction::TriggerSync).await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Daemon should still be running
        assert!(!daemon_handle.is_finished());

        daemon_handle.abort();
        let _ = std::fs::remove_file(&config_path);
    }

    #[tokio::test]
    #[serial]
    async fn test_interval_tick_when_suspended() {
        use tokio::signal::unix::{signal, SignalKind};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        let config_path = Config::default_config_path().unwrap();
        let mut config = create_test_config();
        config.daemon.check_interval_seconds = 1; // Very short for testing
        config.save(&config_path).unwrap();

        let config = Arc::new(RwLock::new(config));

        let (_reload_tx, reload_rx) = mpsc::channel(10);
        let sigterm = signal(SignalKind::terminate()).unwrap();
        let sigint = signal(SignalKind::interrupt()).unwrap();
        let socket_listener = create_test_socket().await;
        let start_time = Instant::now();

        let config_clone = Arc::clone(&config);
        let config_path_clone = config_path.clone();

        let tray_handle = Arc::new(RwLock::new(None));
        let (_tray_action_tx, tray_action_rx) = mpsc::channel(10);
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(true)); // Suspended
        let (tray_action_tx, _tray_action_rx2) = mpsc::channel(10);

        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(
                config_clone,
                config_path_clone,
                reload_rx,
                sigterm,
                sigint,
                socket_listener,
                start_time,
                tray_handle,
                tray_action_rx,
                suspended,
                tray_action_tx,
            ).await;
        });

        // Let it run for multiple intervals while suspended
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Daemon should still be running (intervals skipped due to suspension)
        assert!(!daemon_handle.is_finished());

        daemon_handle.abort();
        let _ = std::fs::remove_file(&config_path);
    }

    #[tokio::test]
    #[serial]
    async fn test_tray_enable_disable_via_config_reload() {
        use tokio::signal::unix::{signal, SignalKind};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        let config_path = Config::default_config_path().unwrap();
        let mut config = create_test_config();
        config.daemon.enable_tray = false; // Start with tray disabled
        config.save(&config_path).unwrap();

        let config = Arc::new(RwLock::new(config));

        let (reload_tx, reload_rx) = mpsc::channel(10);
        let sigterm = signal(SignalKind::terminate()).unwrap();
        let sigint = signal(SignalKind::interrupt()).unwrap();
        let socket_listener = create_test_socket().await;
        let start_time = Instant::now();

        let config_clone = Arc::clone(&config);
        let config_path_clone = config_path.clone();

        let tray_handle = Arc::new(RwLock::new(None));
        let (_tray_action_tx, tray_action_rx) = mpsc::channel(10);
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tray_action_tx, _tray_action_rx2) = mpsc::channel(10);

        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(
                config_clone,
                config_path_clone,
                reload_rx,
                sigterm,
                sigint,
                socket_listener,
                start_time,
                tray_handle,
                tray_action_rx,
                suspended,
                tray_action_tx,
            ).await;
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Enable tray via config reload
        let mut new_config = create_test_config();
        new_config.daemon.enable_tray = true;
        new_config.save(&config_path).unwrap();

        reload_tx.send(()).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Now disable tray again
        let mut new_config = create_test_config();
        new_config.daemon.enable_tray = false;
        new_config.save(&config_path).unwrap();

        reload_tx.send(()).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        daemon_handle.abort();
        let _ = std::fs::remove_file(&config_path);
    }
}
