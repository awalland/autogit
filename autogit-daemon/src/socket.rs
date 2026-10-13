use crate::coordinator::Coordinator;
use anyhow::{Context, Result, bail};
use autogit_shared::{Command, Config, Response, ResponseData};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;
use tracing::error;

/// Holds the instance lock and removes only the socket inode this instance bound.
pub struct SocketOwner {
    _lock: File,
    path: PathBuf,
    identity: Option<(u64, u64)>,
}

impl SocketOwner {
    pub fn acquire(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path.parent().context("Socket path has no parent")?)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension("lock"))?;
        lock.try_lock()
            .context("Another autogit daemon owns the instance lock")?;
        Ok(Self {
            _lock: lock,
            path: path.to_owned(),
            identity: None,
        })
    }

    pub fn bind(&mut self) -> Result<UnixListener> {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path) {
            if !metadata.file_type().is_socket() {
                bail!(
                    "Refusing to replace non-socket path: {}",
                    self.path.display()
                );
            }
            if std::os::unix::net::UnixStream::connect(&self.path).is_ok() {
                bail!("Another daemon is listening on {}", self.path.display());
            }
            std::fs::remove_file(&self.path)?;
        }
        let listener = UnixListener::bind(&self.path)?;
        let metadata = std::fs::symlink_metadata(&self.path)?;
        self.identity = Some((metadata.dev(), metadata.ino()));
        Ok(listener)
    }
}

impl Drop for SocketOwner {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
            && self.identity == Some((metadata.dev(), metadata.ino()))
            && let Err(error) = std::fs::remove_file(&self.path)
        {
            error!(%error, "Failed to remove owned socket");
        }
    }
}

pub async fn handle_connection(
    stream: UnixStream,
    config: Arc<RwLock<Config>>,
    start_time: Instant,
    coordinator: Coordinator,
) {
    if let Err(error) = handle_connection_impl(stream, config, start_time, coordinator).await {
        error!(%error, "Socket connection failed");
    }
}

async fn handle_connection_impl(
    stream: UnixStream,
    config: Arc<RwLock<Config>>,
    start_time: Instant,
    coordinator: Coordinator,
) -> Result<()> {
    let mut reader = BufReader::new(stream);
    // Bound both idle clients and command size; a client cannot retain unlimited memory.
    let mut line = Vec::new();
    use tokio::io::AsyncReadExt;
    let size = tokio::time::timeout(
        Duration::from_secs(10),
        (&mut reader).take(4097).read_until(b'\n', &mut line),
    )
    .await??;
    if size == 0 {
        return Ok(());
    }
    let response = if size > 4096 || line.last() != Some(&b'\n') {
        Response::error("Invalid command length")
    } else {
        match Command::from_json(std::str::from_utf8(&line)?) {
            Ok(Command::Ping) => Response::ok("pong"),
            Ok(Command::Trigger) => coordinator.trigger().await,
            Ok(Command::Status) => {
                let config = config.read().await.clone();
                let state = coordinator.state.borrow().clone();
                Response::ok_with_data(
                    "Daemon status",
                    ResponseData::Status {
                        uptime_seconds: start_time.elapsed().as_secs(),
                        check_interval_seconds: config.daemon.check_interval_seconds,
                        repositories_count: config.repositories.len(),
                        config: config.clone(),
                        suspended: coordinator.suspended.load(Ordering::SeqCst),
                        syncing: state.syncing,
                        details: state
                            .details
                            .into_iter()
                            .filter(|detail| {
                                config
                                    .repositories
                                    .iter()
                                    .any(|repo| repo.path == detail.path)
                            })
                            .collect(),
                    },
                )
            }
            Ok(Command::Suspend) => {
                coordinator.set_suspended(true);
                Response::ok("Daemon suspended")
            }
            Ok(Command::Resume) => {
                coordinator.set_suspended(false);
                coordinator.schedule(None);
                Response::ok("Daemon resumed")
            }
            Err(error) => Response::error(format!("Invalid command: {error}")),
        }
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        reader.get_mut().write_all(response.to_json()?.as_bytes()),
    )
    .await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn competing_instance_cannot_replace_socket() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("daemon.sock");
        let mut owner = SocketOwner::acquire(&path).unwrap();
        let listener = owner.bind().unwrap();
        assert!(SocketOwner::acquire(&path).is_err());
        assert!(UnixStream::connect(&path).await.is_ok());
        drop(listener);
        drop(owner);
        assert!(!path.exists());
        assert!(SocketOwner::acquire(&path).is_ok());
    }

    #[tokio::test]
    async fn cleanup_preserves_replacement_socket() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("daemon.sock");
        let mut owner = SocketOwner::acquire(&path).unwrap();
        let listener = owner.bind().unwrap();
        std::fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        drop(owner);
        assert!(path.exists());
        drop((listener, replacement));
    }

    #[tokio::test]
    async fn stale_socket_is_replaced_but_regular_file_is_preserved() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("daemon.sock");
        drop(UnixListener::bind(&path).unwrap());
        let mut owner = SocketOwner::acquire(&path).unwrap();
        drop(owner.bind().unwrap());
        drop(owner);
        std::fs::write(&path, "valuable content").unwrap();
        let mut owner = SocketOwner::acquire(&path).unwrap();
        assert!(owner.bind().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "valuable content");
    }
}
