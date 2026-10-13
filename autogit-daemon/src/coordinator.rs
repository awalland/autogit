use autogit_shared::{Config, RepoDetail, Response, ResponseData, ResponseStatus};
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{RwLock, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{error, info};

#[derive(Clone, Default)]
pub struct SyncState {
    pub syncing: bool,
    pub details: Vec<RepoDetail>,
    pub completed_cycles: u64,
}

struct Request {
    paths: Option<Vec<PathBuf>>,
    reply: Option<oneshot::Sender<Response>>,
}

/// One worker owns all repository mutations, regardless of their trigger.
#[derive(Clone)]
pub struct Coordinator {
    requests: mpsc::Sender<Request>,
    pub state: watch::Receiver<SyncState>,
    pub suspended: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    active_cancel: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

type SyncFuture = Pin<Box<dyn Future<Output = anyhow::Result<crate::git::SyncOutcome>> + Send>>;

trait RepositorySync: Send + Sync {
    fn run(&self, repository: autogit_shared::Repository, cancel: Arc<AtomicBool>) -> SyncFuture;
}

struct GitSync;

impl RepositorySync for GitSync {
    fn run(&self, repository: autogit_shared::Repository, cancel: Arc<AtomicBool>) -> SyncFuture {
        Box::pin(async move { crate::git::sync_repository(&repository, cancel).await })
    }
}

impl Coordinator {
    pub fn start(
        config: Arc<RwLock<Config>>,
        suspended: Arc<AtomicBool>,
    ) -> (Self, JoinHandle<()>) {
        Self::start_with(config, suspended, Arc::new(GitSync))
    }

    fn start_with(
        config: Arc<RwLock<Config>>,
        suspended: Arc<AtomicBool>,
        runner: Arc<dyn RepositorySync>,
    ) -> (Self, JoinHandle<()>) {
        let (requests, receiver) = mpsc::channel(16);
        let (state_tx, state) = watch::channel(SyncState::default());
        let coordinator = Self {
            requests,
            state,
            suspended,
            stopping: Arc::new(AtomicBool::new(false)),
            active_cancel: Arc::new(Mutex::new(None)),
        };
        let worker = tokio::spawn(coordinator.clone().work(config, receiver, state_tx, runner));
        (coordinator, worker)
    }

    pub fn schedule(&self, paths: Option<Vec<PathBuf>>) {
        // Periodic requests are coalesced when the bounded queue is full.
        let _ = self.requests.try_send(Request { paths, reply: None });
    }

    pub async fn trigger(&self) -> Response {
        if self.suspended.load(Ordering::SeqCst) {
            return Response::error("Daemon is suspended");
        }
        let (reply, result) = oneshot::channel();
        if self
            .requests
            .try_send(Request {
                paths: None,
                reply: Some(reply),
            })
            .is_err()
        {
            return Response::error("Sync queue is full or daemon is shutting down");
        }
        result
            .await
            .unwrap_or_else(|_| Response::error("Daemon is shutting down"))
    }

    pub fn set_suspended(&self, suspended: bool) {
        self.suspended.store(suspended, Ordering::SeqCst);
        if suspended {
            self.cancel_active();
        }
        info!(suspended, "Updated daemon suspension");
    }

    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.cancel_active();
        // Wake an idle worker without depending on sender lifetimes.
        self.schedule(None);
    }

    fn cancel_active(&self) {
        if let Some(cancel) = self.active_cancel.lock().unwrap().as_ref() {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    async fn work(
        self,
        config: Arc<RwLock<Config>>,
        mut requests: mpsc::Receiver<Request>,
        state_tx: watch::Sender<SyncState>,
        runner: Arc<dyn RepositorySync>,
    ) {
        while let Some(request) = requests.recv().await {
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            if self.suspended.load(Ordering::SeqCst) {
                if let Some(reply) = request.reply {
                    let _ = reply.send(Response::error("Daemon is suspended"));
                }
                continue;
            }
            let cancel = Arc::new(AtomicBool::new(false));
            *self.active_cancel.lock().unwrap() = Some(cancel.clone());
            // Suspension can race with installing the active cancellation token.
            if self.suspended.load(Ordering::SeqCst) || self.stopping.load(Ordering::SeqCst) {
                cancel.store(true, Ordering::SeqCst);
            }
            state_tx.send_modify(|state| state.syncing = true);
            let repositories = config.read().await.repositories.clone();
            let mut details = Vec::new();
            for repo in repositories {
                if !repo.auto_commit
                    || request
                        .paths
                        .as_ref()
                        .is_some_and(|paths| !paths.contains(&repo.path))
                {
                    continue;
                }
                // Recheck the active configuration between repositories, without holding a lock over Git.
                if !config
                    .read()
                    .await
                    .repositories
                    .iter()
                    .any(|active| active.path == repo.path && active.auto_commit)
                {
                    continue;
                }
                if cancel.load(Ordering::SeqCst) || self.suspended.load(Ordering::SeqCst) {
                    break;
                }
                let result = runner.run(repo.clone(), cancel.clone()).await;
                let detail = match result {
                    Ok(outcome) => RepoDetail {
                        path: repo.path,
                        committed: outcome.committed,
                        files_changed: outcome.files_changed,
                        error: outcome.error_message(),
                    },
                    Err(error) => {
                        error!(path = %repo.path.display(), %error, "Repository sync failed");
                        RepoDetail {
                            path: repo.path,
                            committed: false,
                            files_changed: None,
                            error: Some(format!("{error:#}")),
                        }
                    }
                };
                details.push(detail);
            }
            let interrupted =
                cancel.load(Ordering::SeqCst) || self.suspended.load(Ordering::SeqCst);
            let failed = interrupted || details.iter().any(|detail| detail.error.is_some());
            let committed = details.iter().filter(|detail| detail.committed).count();
            let response = Response {
                status: if failed {
                    ResponseStatus::Error
                } else {
                    ResponseStatus::Ok
                },
                message: if interrupted {
                    "Sync interrupted".to_owned()
                } else {
                    format!(
                        "Checked {} repositories, committed changes in {committed}",
                        details.len()
                    )
                },
                data: Some(ResponseData::Trigger {
                    repos_checked: details.len(),
                    repos_committed: committed,
                    details: details.clone(),
                }),
            };
            let active_paths = config
                .read()
                .await
                .repositories
                .iter()
                .map(|repo| repo.path.clone())
                .collect::<HashSet<_>>();
            state_tx.send_modify(|state| {
                state.syncing = false;
                state
                    .details
                    .retain(|detail| active_paths.contains(&detail.path));
                for detail in &details {
                    state.details.retain(|old| old.path != detail.path);
                    if active_paths.contains(&detail.path) {
                        state.details.push(detail.clone());
                    }
                }
                state.completed_cycles += 1;
            });
            *self.active_cancel.lock().unwrap() = None;
            info!(failed, committed, "Repository sync cycle finished");
            #[cfg(not(test))]
            if failed && !interrupted {
                let notification = notify_rust::Notification::new().summary("Autogit sync failed").body("One or more repositories could not synchronise. See daemon status for details.").finalize();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    notification.show_async(),
                )
                .await;
            }
            if let Some(reply) = request.reply {
                let _ = reply.send(response);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autogit_shared::{DaemonConfig, Repository};
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::sync::Semaphore;

    struct ControlledSync {
        entered: mpsc::UnboundedSender<PathBuf>,
        permits: Arc<Semaphore>,
        fail_push: bool,
    }

    impl RepositorySync for ControlledSync {
        fn run(&self, repository: Repository, cancel: Arc<AtomicBool>) -> SyncFuture {
            let entered = self.entered.clone();
            let permits = self.permits.clone();
            let fail = self.fail_push;
            Box::pin(async move {
                entered.send(repository.path).unwrap();
                loop {
                    tokio::select! {
                        permit = permits.acquire() => { permit.unwrap().forget(); break; }
                        _ = tokio::time::sleep(Duration::from_millis(5)) => {
                            if cancel.load(Ordering::SeqCst) { anyhow::bail!("Cancelled"); }
                        }
                    }
                }
                Ok(crate::git::SyncOutcome {
                    committed: true,
                    files_changed: Some(2),
                    pull: crate::git::OperationOutcome::Succeeded,
                    push: if fail {
                        crate::git::OperationOutcome::Failed("offline".to_owned())
                    } else {
                        crate::git::OperationOutcome::Succeeded
                    },
                })
            })
        }
    }

    fn repository(path: &str) -> Repository {
        Repository {
            path: PathBuf::from(path),
            auto_commit: true,
            commit_message_template: "update".to_owned(),
        }
    }

    struct TestHarness {
        coordinator: Coordinator,
        worker: JoinHandle<()>,
        config: Arc<RwLock<Config>>,
        entered: mpsc::UnboundedReceiver<PathBuf>,
        permits: Arc<Semaphore>,
    }

    fn setup(count: usize, fail: bool) -> TestHarness {
        let config = Arc::new(RwLock::new(Config {
            daemon: DaemonConfig::default(),
            repositories: (0..count)
                .map(|index| repository(&format!("/repo{index}")))
                .collect(),
        }));
        let (entered, receiver) = mpsc::unbounded_channel();
        let permits = Arc::new(Semaphore::new(0));
        let runner = Arc::new(ControlledSync {
            entered,
            permits: permits.clone(),
            fail_push: fail,
        });
        let (coordinator, worker) =
            Coordinator::start_with(config.clone(), Arc::new(AtomicBool::new(false)), runner);
        TestHarness {
            coordinator,
            worker,
            config,
            entered: receiver,
            permits,
        }
    }

    async fn command(
        config: Arc<RwLock<Config>>,
        coordinator: Coordinator,
        command: autogit_shared::Command,
    ) -> Response {
        let (mut client, server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(crate::socket::handle_connection(
            server,
            config,
            Instant::now(),
            coordinator,
        ));
        client
            .write_all(command.to_json().unwrap().as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_millis(250),
            BufReader::new(client).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        task.await.unwrap();
        Response::from_json(&line).unwrap()
    }

    #[tokio::test]
    async fn concurrent_triggers_are_serialized_and_publish_failures() {
        let TestHarness {
            coordinator,
            worker,
            mut entered,
            permits,
            ..
        } = setup(1, true);
        let first = tokio::spawn({
            let coordinator = coordinator.clone();
            async move { coordinator.trigger().await }
        });
        assert_eq!(entered.recv().await.unwrap(), PathBuf::from("/repo0"));
        let second = tokio::spawn({
            let coordinator = coordinator.clone();
            async move { coordinator.trigger().await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(30), entered.recv())
                .await
                .is_err()
        );
        permits.add_permits(1);
        let response = first.await.unwrap();
        assert_eq!(response.status, ResponseStatus::Error);
        let Some(ResponseData::Trigger {
            details,
            repos_committed,
            ..
        }) = response.data
        else {
            panic!("Missing trigger data")
        };
        assert_eq!(repos_committed, 1);
        assert!(details[0].error.as_ref().unwrap().contains("offline"));
        entered.recv().await.unwrap();
        permits.add_permits(1);
        second.await.unwrap();
        assert_eq!(coordinator.state.borrow().completed_cycles, 2);
        assert!(coordinator.state.borrow().details[0].error.is_some());
        coordinator.shutdown();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn controls_remain_responsive_and_suspension_stops_remaining_repositories() {
        let TestHarness {
            coordinator,
            worker,
            config,
            mut entered,
            ..
        } = setup(2, false);
        let sync = tokio::spawn({
            let coordinator = coordinator.clone();
            async move { coordinator.trigger().await }
        });
        entered.recv().await.unwrap();
        let status = command(
            config.clone(),
            coordinator.clone(),
            autogit_shared::Command::Status,
        )
        .await;
        let Some(ResponseData::Status {
            syncing,
            config: active,
            ..
        }) = status.data
        else {
            panic!("Missing status")
        };
        assert!(syncing);
        assert_eq!(active.repositories.len(), 2);
        assert_eq!(
            command(
                config.clone(),
                coordinator.clone(),
                autogit_shared::Command::Ping
            )
            .await
            .status,
            ResponseStatus::Ok
        );
        assert_eq!(
            command(
                config.clone(),
                coordinator.clone(),
                autogit_shared::Command::Suspend
            )
            .await
            .status,
            ResponseStatus::Ok
        );
        assert_eq!(sync.await.unwrap().status, ResponseStatus::Error);
        assert!(entered.try_recv().is_err());
        config.write().await.repositories.push(repository("/new"));
        coordinator.schedule(Some(vec![PathBuf::from("/new")]));
        assert_eq!(coordinator.trigger().await.status, ResponseStatus::Error);
        assert!(entered.try_recv().is_err());
        coordinator.shutdown();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_active_worker_without_detaching_it() {
        let TestHarness {
            coordinator,
            worker,
            mut entered,
            ..
        } = setup(1, false);
        coordinator.schedule(None);
        entered.recv().await.unwrap();
        coordinator.shutdown();
        tokio::time::timeout(Duration::from_millis(250), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(!coordinator.state.borrow().syncing);
    }
}
