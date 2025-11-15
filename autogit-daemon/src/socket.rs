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

#[cfg(test)]
mod tests {
    use super::*;
    use autogit_shared::{Config, DaemonConfig, Repository, ResponseStatus};
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::TempDir;
    use std::env;
    use serial_test::serial;

    fn create_test_config() -> Config {
        Config {
            daemon: DaemonConfig {
                check_interval_seconds: 300,
            },
            repositories: vec![],
        }
    }

    fn create_test_config_with_repos() -> Config {
        Config {
            daemon: DaemonConfig {
                check_interval_seconds: 120,
            },
            repositories: vec![
                Repository {
                    path: PathBuf::from("/test/repo1"),
                    auto_commit: true,
                    commit_message_template: "Auto: {timestamp}".to_owned(),
                },
                Repository {
                    path: PathBuf::from("/test/repo2"),
                    auto_commit: false, // Disabled
                    commit_message_template: "Update".to_owned(),
                },
                Repository {
                    path: PathBuf::from("/test/repo3"),
                    auto_commit: true,
                    commit_message_template: "Checkpoint".to_owned(),
                },
            ],
        }
    }

    #[tokio::test]
    async fn test_handle_status_command_empty_config() {
        let config = Arc::new(RwLock::new(create_test_config()));
        let start_time = Instant::now();

        let response = handle_status_command(config, start_time).await;

        assert_eq!(response.status, ResponseStatus::Ok);
        assert_eq!(response.message, "Daemon status");

        if let Some(ResponseData::Status { uptime_seconds, check_interval_seconds, repositories_count }) = response.data {
            assert_eq!(uptime_seconds, 0); // Just started
            assert_eq!(check_interval_seconds, 300);
            assert_eq!(repositories_count, 0);
        } else {
            panic!("Expected Status response data");
        }
    }

    #[tokio::test]
    async fn test_handle_status_command_with_repos() {
        let config = Arc::new(RwLock::new(create_test_config_with_repos()));
        let start_time = Instant::now();

        tokio::time::sleep(Duration::from_millis(10)).await;

        let response = handle_status_command(config, start_time).await;

        assert_eq!(response.status, ResponseStatus::Ok);

        if let Some(ResponseData::Status { uptime_seconds, check_interval_seconds, repositories_count }) = response.data {
            // uptime_seconds is u64, so it's always >= 0, just verify it exists
            let _ = uptime_seconds;
            assert_eq!(check_interval_seconds, 120);
            assert_eq!(repositories_count, 3);
        } else {
            panic!("Expected Status response data");
        }
    }

    #[tokio::test]
    async fn test_handle_status_command_uptime() {
        let config = Arc::new(RwLock::new(create_test_config()));
        let start_time = Instant::now() - Duration::from_secs(5);

        let response = handle_status_command(config, start_time).await;

        if let Some(ResponseData::Status { uptime_seconds, .. }) = response.data {
            assert!(uptime_seconds >= 5);
            assert!(uptime_seconds < 10); // Should be around 5 seconds
        } else {
            panic!("Expected Status response data");
        }
    }

    #[tokio::test]
    async fn test_handle_trigger_command_empty_config() {
        let config = Arc::new(RwLock::new(create_test_config()));

        let response = handle_trigger_command(config).await;

        assert_eq!(response.status, ResponseStatus::Ok);
        assert!(response.message.contains("0 repositories"));

        if let Some(ResponseData::Trigger { repos_checked, repos_committed, details }) = response.data {
            assert_eq!(repos_checked, 0);
            assert_eq!(repos_committed, 0);
            assert_eq!(details.len(), 0);
        } else {
            panic!("Expected Trigger response data");
        }
    }

    #[tokio::test]
    async fn test_handle_trigger_command_skips_disabled_repos() {
        let config = Arc::new(RwLock::new(create_test_config_with_repos()));

        // This will fail to actually commit (repos don't exist), but we're testing
        // that it only processes enabled repos
        let response = handle_trigger_command(config).await;

        assert_eq!(response.status, ResponseStatus::Ok);

        if let Some(ResponseData::Trigger { repos_checked, details, .. }) = response.data {
            // Should only check repo1 and repo3 (auto_commit=true)
            // repo2 is disabled (auto_commit=false)
            assert_eq!(repos_checked, 2);
            assert_eq!(details.len(), 2);

            // Verify the paths are correct
            assert_eq!(details[0].path, PathBuf::from("/test/repo1"));
            assert_eq!(details[1].path, PathBuf::from("/test/repo3"));
        } else {
            panic!("Expected Trigger response data");
        }
    }

    #[tokio::test]
    async fn test_handle_trigger_command_response_format() {
        let config = Arc::new(RwLock::new(create_test_config_with_repos()));

        let response = handle_trigger_command(config).await;

        assert_eq!(response.status, ResponseStatus::Ok);
        assert!(response.message.contains("Checked"));
        assert!(response.message.contains("repositories"));
        assert!(response.data.is_some());
    }

    #[test]
    #[serial]
    fn test_cleanup_socket_no_socket() {
        // Setup temp config dir
        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Socket doesn't exist, should not error
        cleanup_socket();

        // Verify it didn't create anything
        let socket_path = socket_path().unwrap();
        assert!(!socket_path.exists());
    }

    #[test]
    #[serial]
    fn test_cleanup_socket_removes_existing() {
        // Setup temp config dir
        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Create the socket directory and a fake socket file
        let socket_path = socket_path().unwrap();
        std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
        std::fs::write(&socket_path, b"fake socket").unwrap();

        assert!(socket_path.exists());

        // Clean up
        cleanup_socket();

        // Verify it was removed
        assert!(!socket_path.exists());
    }

    #[tokio::test]
    #[serial]
    async fn test_create_listener_creates_parent_dir() {
        // Setup temp config dir
        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        let socket_path = socket_path().unwrap();
        assert!(!socket_path.exists());
        assert!(!socket_path.parent().unwrap().exists());

        // Create listener (this will create the directory)
        let listener = create_listener();
        assert!(listener.is_ok());

        // Verify parent directory was created
        assert!(socket_path.parent().unwrap().exists());

        // Cleanup
        drop(listener);
        cleanup_socket();
    }

    #[tokio::test]
    #[serial]
    async fn test_create_listener_removes_stale_socket() {
        // Setup temp config dir
        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        let socket_path = socket_path().unwrap();
        std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();

        // Create a stale socket file
        std::fs::write(&socket_path, b"stale socket").unwrap();
        assert!(socket_path.exists());

        // Create listener should remove the stale file and create a new one
        let listener = create_listener();
        assert!(listener.is_ok());

        // Socket should still exist (as a real socket now)
        assert!(socket_path.exists());

        // Cleanup
        drop(listener);
        cleanup_socket();
    }

    #[tokio::test]
    async fn test_handle_trigger_different_repo_counts() {
        // Test with 0 repos
        let config0 = Arc::new(RwLock::new(create_test_config()));
        let response0 = handle_trigger_command(config0).await;
        if let Some(ResponseData::Trigger { repos_checked, .. }) = response0.data {
            assert_eq!(repos_checked, 0);
        }

        // Test with 2 enabled repos (out of 3 total)
        let config2 = Arc::new(RwLock::new(create_test_config_with_repos()));
        let response2 = handle_trigger_command(config2).await;
        if let Some(ResponseData::Trigger { repos_checked, .. }) = response2.data {
            assert_eq!(repos_checked, 2);
        }

        // Test with all repos enabled
        let mut config_all = create_test_config_with_repos();
        config_all.repositories[1].auto_commit = true; // Enable repo2
        let config_all = Arc::new(RwLock::new(config_all));
        let response_all = handle_trigger_command(config_all).await;
        if let Some(ResponseData::Trigger { repos_checked, .. }) = response_all.data {
            assert_eq!(repos_checked, 3);
        }
    }

    #[tokio::test]
    async fn test_handle_status_different_intervals() {
        let intervals = vec![1, 60, 120, 300, 600, 3600];

        for interval in intervals {
            let mut config = create_test_config();
            config.daemon.check_interval_seconds = interval;
            let config = Arc::new(RwLock::new(config));
            let start_time = Instant::now();

            let response = handle_status_command(config, start_time).await;

            if let Some(ResponseData::Status { check_interval_seconds, .. }) = response.data {
                assert_eq!(check_interval_seconds, interval);
            } else {
                panic!("Expected Status response data");
            }
        }
    }

    #[tokio::test]
    async fn test_response_messages() {
        // Test status message
        let config = Arc::new(RwLock::new(create_test_config()));
        let response = handle_status_command(config.clone(), Instant::now()).await;
        assert_eq!(response.message, "Daemon status");

        // Test trigger message format
        let response = handle_trigger_command(config).await;
        assert!(response.message.starts_with("Checked"));
        assert!(response.message.contains("repositories"));
        assert!(response.message.contains("committed changes in"));
    }
}
