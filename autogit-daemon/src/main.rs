mod git;

use anyhow::{Context, Result};
use autogit_shared::Config;
use notify::{Watcher, RecursiveMode, Event};
use std::path::PathBuf;
use std::sync::Arc;
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

    info!("Starting autogit-daemon");

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

    // Set up signal handling for graceful shutdown
    let sigterm = signal(SignalKind::terminate())
        .context("Failed to create SIGTERM handler")?;
    let sigint = signal(SignalKind::interrupt())
        .context("Failed to create SIGINT handler")?;

    // Start the main daemon loop
    run_daemon(config, config_path_clone, reload_rx, sigterm, sigint).await?;

    // Keep watcher alive until daemon exits
    drop(watcher);

    info!("autogit-daemon shutting down");
    Ok(())
}

async fn run_daemon(
    config: Arc<RwLock<Config>>,
    config_path: PathBuf,
    mut reload_rx: mpsc::Receiver<()>,
    mut sigterm: tokio::signal::unix::Signal,
    mut sigint: tokio::signal::unix::Signal,
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
