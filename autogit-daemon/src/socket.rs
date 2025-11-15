use anyhow::{Context, Result};
use autogit_shared::{Command, Response, ResponseData, RepoDetail, socket_path};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;
use tracing::{info, error, warn};

use crate::Config;

/// Start the Unix socket listener
pub fn create_listener() -> Result<UnixListener> {
    let socket_path = socket_path()
        .context("Failed to get socket path")?;

    // Remove old socket file if it exists (from previous unclean shutdown)
    if socket_path.exists() {
        warn!("Removing stale socket file: {}", socket_path.display());
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("Failed to remove stale socket: {}", socket_path.display()))?;
    }

    // Ensure parent directory exists
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create socket directory: {}", parent.display()))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("Failed to bind Unix socket: {}", socket_path.display()))?;

    info!("Listening on Unix socket: {}", socket_path.display());

    Ok(listener)
}

/// Clean up the socket file on shutdown
pub fn cleanup_socket() {
    if let Ok(path) = socket_path() {
        if path.exists() {
            if let Err(e) = std::fs::remove_file(&path) {
                error!("Failed to remove socket file: {:#}", e);
            } else {
                info!("Removed socket file: {}", path.display());
            }
        }
    }
}

/// Handle an incoming connection
pub async fn handle_connection(
    stream: UnixStream,
    config: Arc<RwLock<Config>>,
    start_time: Instant,
) {
    if let Err(e) = handle_connection_impl(stream, config, start_time).await {
        error!("Error handling socket connection: {:#}", e);
    }
}

async fn handle_connection_impl(
    stream: UnixStream,
    config: Arc<RwLock<Config>>,
    start_time: Instant,
) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    // Read one line (JSON command)
    reader.read_line(&mut line).await
        .context("Failed to read command from socket")?;

    if line.is_empty() {
        // Connection closed
        return Ok(());
    }

    // Parse command
    let command = Command::from_json(&line)
        .context("Failed to parse command")?;

    info!("Received socket command: {:?}", command);

    // Execute command and get response
    let response = match command {
        Command::Ping => {
            Response::ok("pong")
        }
        Command::Status => {
            handle_status_command(config, start_time).await
        }
        Command::Trigger => {
            handle_trigger_command(config).await
        }
    };

    // Send response
    let response_json = response.to_json()
        .context("Failed to serialize response")?;

    let stream = reader.into_inner();
    let mut stream = stream;
    stream.write_all(response_json.as_bytes()).await
        .context("Failed to write response to socket")?;

    stream.flush().await
        .context("Failed to flush socket")?;

    Ok(())
}

async fn handle_status_command(config: Arc<RwLock<Config>>, start_time: Instant) -> Response {
    let cfg = config.read().await;
    let uptime = start_time.elapsed().as_secs();

    Response::ok_with_data(
        "Daemon status",
        ResponseData::Status {
            uptime_seconds: uptime,
            check_interval_seconds: cfg.daemon.check_interval_seconds,
            repositories_count: cfg.repositories.len(),
        },
    )
}

async fn handle_trigger_command(config: Arc<RwLock<Config>>) -> Response {
    let cfg = config.read().await;

    let mut repos_checked = 0;
    let mut repos_committed = 0;
    let mut details = Vec::new();

    // Process each repository
    for repo in &cfg.repositories {
        if !repo.auto_commit {
            continue;
        }

        repos_checked += 1;

        match crate::git::check_and_commit(repo).await {
            Ok(committed) => {
                if committed {
                    repos_committed += 1;
                }

                details.push(RepoDetail {
                    path: repo.path.clone(),
                    committed,
                    files_changed: if committed { Some(0) } else { None }, // TODO: track actual file count
                    error: None,
                });

                if committed {
                    info!("Committed changes in: {}", repo.path.display());
                }
            }
            Err(e) => {
                error!("Error processing repository {}: {:#}", repo.path.display(), e);
                details.push(RepoDetail {
                    path: repo.path.clone(),
                    committed: false,
                    files_changed: None,
                    error: Some(format!("{:#}", e)),
                });
            }
        }
    }

    info!("Manual trigger complete: checked {}, committed {}", repos_checked, repos_committed);

    Response::ok_with_data(
        format!("Checked {} repositories, committed changes in {}", repos_checked, repos_committed),
        ResponseData::Trigger {
            repos_checked,
            repos_committed,
            details,
        },
    )
}
