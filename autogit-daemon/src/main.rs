mod git;
mod socket;

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

    // Set up signal handling for graceful shutdown
    let sigterm = signal(SignalKind::terminate())
        .context("Failed to create SIGTERM handler")?;
    let sigint = signal(SignalKind::interrupt())
        .context("Failed to create SIGINT handler")?;

    // Track daemon start time for uptime reporting
    let start_time = Instant::now();

    // Start the main daemon loop
    run_daemon(config, config_path_clone, reload_rx, sigterm, sigint, socket_listener, start_time).await?;

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
                tokio::spawn(async move {
                    socket::handle_connection(stream, config_clone, start_time).await;
                });
            }

            _ = interval.tick() => {
                // Normal check cycle
                let cfg = config.read().await;

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
                        }
                    }
                }
            }

            Some(_) = reload_rx.recv() => {
                // Config file changed, reload it
                info!("Reloading configuration from: {}", config_path.display());

                match Config::load(&config_path) {
                    Ok(new_config) => {
                        let old_interval = {
                            let cfg = config.read().await;
                            cfg.daemon.check_interval_seconds
                        };

                        let new_interval = new_config.daemon.check_interval_seconds;

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

                        info!("Configuration reloaded successfully with {} repositories",
                              config.read().await.repositories.len());

                        // Update interval if it changed
                        if old_interval != new_interval {
                            info!("Check interval changed from {}s to {}s, updating timer",
                                  old_interval, new_interval);
                            interval = tokio::time::interval(std::time::Duration::from_secs(new_interval));
                        }
                    }
                    Err(e) => {
                        error!("Failed to reload configuration: {:#}", e);
                        warn!("Keeping previous configuration");
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
}
