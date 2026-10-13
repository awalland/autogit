use autogit_shared::{
    Command, Config, DaemonConfig, RepoDetail, Repository, Response, ResponseData, ResponseStatus,
};
use git2::Repository as GitRepository;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Daemon {
    child: Child,
    config_path: PathBuf,
    socket_path: PathBuf,
    log_path: PathBuf,
}

struct DaemonStatus {
    repositories: usize,
    interval: u64,
    suspended: bool,
    syncing: bool,
    details: Vec<RepoDetail>,
}

impl Daemon {
    fn start(directory: &Path, config: &Config, git_wrapper: Option<&Path>) -> Self {
        let config_path = directory.join("autogit/config.toml");
        config.save(&config_path).unwrap();
        let log_path = directory.join("daemon.log");
        let log = File::create(&log_path).unwrap();
        let mut command = daemon_command(directory);
        command.stdout(log.try_clone().unwrap()).stderr(log);
        if let Some(wrapper) = git_wrapper {
            let mut paths = vec![wrapper.to_owned()];
            paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
            command.env("PATH", std::env::join_paths(paths).unwrap());
            command.env("AUTOGIT_TEST_DIRECTORY", directory);
        }
        let mut daemon = Self {
            child: command.spawn().unwrap(),
            config_path,
            socket_path: directory.join("autogit/daemon.sock"),
            log_path,
        };
        wait_until(|| {
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "{}",
                daemon.logs()
            );
            daemon.socket_path.exists()
        });
        daemon
    }

    fn request(&self, command: Command) -> Response {
        let mut stream = UnixStream::connect(&self.socket_path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .write_all(command.to_json().unwrap().as_bytes())
            .unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        Response::from_json(&response).unwrap()
    }

    fn status(&self) -> DaemonStatus {
        match self.request(Command::Status).data.unwrap() {
            ResponseData::Status {
                repositories_count,
                check_interval_seconds,
                suspended,
                syncing,
                details,
                ..
            } => DaemonStatus {
                repositories: repositories_count,
                interval: check_interval_seconds,
                suspended,
                syncing,
                details,
            },
            _ => panic!("Expected daemon status"),
        }
    }

    fn logs(&self) -> String {
        fs::read_to_string(&self.log_path).unwrap()
    }

    fn stop(&mut self) {
        // SAFETY: the PID belongs to the child managed by this fixture.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) },
            0
        );
        wait_until(|| self.child.try_wait().unwrap().is_some());
        assert!(self.child.wait().unwrap().success(), "{}", self.logs());
        assert!(!self.socket_path.exists());
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn daemon_command(directory: &Path) -> ProcessCommand {
    let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_autogit-daemon"));
    command
        .current_dir(directory)
        .env("XDG_CONFIG_HOME", directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/autogit-test-bus",
        )
        .stdin(Stdio::null());
    command
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for daemon state"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn config(repositories: Vec<Repository>) -> Config {
    Config {
        daemon: DaemonConfig {
            check_interval_seconds: 3600,
            enable_tray: false,
        },
        repositories,
    }
}

fn repository(path: &Path) -> Repository {
    let repo = GitRepository::init(path).unwrap();
    let mut git_config = repo.config().unwrap();
    git_config.set_str("user.name", "Autogit tests").unwrap();
    git_config
        .set_str("user.email", "tests@example.invalid")
        .unwrap();
    Repository {
        path: path.to_owned(),
        auto_commit: true,
        commit_message_template: "Test commit".to_owned(),
    }
}

#[test]
fn singleton_lock_precedes_repository_initialization() {
    let directory = TempDir::new().unwrap();
    let mut daemon = Daemon::start(directory.path(), &config(vec![]), None);
    daemon.request(Command::Suspend);
    let repo = repository(&directory.path().join("repo"));
    fs::write(repo.path.join("file"), "uncommitted").unwrap();
    config(vec![repo.clone()])
        .save(&daemon.config_path)
        .unwrap();
    wait_until(|| daemon.status().repositories == 1);
    let result = daemon_command(directory.path()).output().unwrap();
    assert!(!result.status.success());
    assert!(GitRepository::open(&repo.path).unwrap().head().is_err());
    assert_eq!(daemon.request(Command::Ping).message, "pong");
    daemon.stop();
}

#[test]
fn configuration_replacements_and_invalid_updates_preserve_live_state() {
    let directory = TempDir::new().unwrap();
    let mut daemon = Daemon::start(directory.path(), &config(vec![]), None);
    for interval in [3601, 3602] {
        let mut updated = config(vec![]);
        updated.daemon.check_interval_seconds = interval;
        updated.save(&daemon.config_path).unwrap();
        wait_until(|| daemon.status().interval == interval);
    }
    fs::write(
        &daemon.config_path,
        "[daemon]\ncheck_interval_seconds = 0\n",
    )
    .unwrap();
    wait_until(|| daemon.logs().contains("Configuration reload failed"));
    assert_eq!(daemon.status().interval, 3602);
    daemon.stop();
}

#[test]
fn startup_git_work_allows_controls_and_shutdown_cancels_children() {
    let directory = TempDir::new().unwrap();
    let repo = repository(&directory.path().join("repo"));
    GitRepository::open(&repo.path)
        .unwrap()
        .remote("origin", "/nonexistent/autogit-test-remote")
        .unwrap();
    let wrapper = directory.path().join("bin");
    fs::create_dir(&wrapper).unwrap();
    let executable = wrapper.join("git");
    fs::write(
        &executable,
        "#!/bin/sh\ntouch \"$AUTOGIT_TEST_DIRECTORY/started\"\nwhile :; do sleep 1; done\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let mut daemon = Daemon::start(directory.path(), &config(vec![repo]), Some(&wrapper));
    wait_until(|| directory.path().join("started").exists());
    assert!(daemon.status().syncing);
    assert_eq!(daemon.request(Command::Ping).message, "pong");
    assert_eq!(daemon.request(Command::Suspend).status, ResponseStatus::Ok);
    wait_until(|| !daemon.status().syncing);
    assert!(daemon.status().suspended);
    assert_eq!(
        daemon.request(Command::Trigger).status,
        ResponseStatus::Error
    );
    daemon.request(Command::Resume);
    wait_until(|| daemon.status().syncing);
    daemon.stop();
}

#[test]
fn failures_are_visible_and_removed_repositories_leave_status() {
    let directory = TempDir::new().unwrap();
    let repo = repository(&directory.path().join("repo"));
    GitRepository::open(&repo.path)
        .unwrap()
        .remote("origin", "/nonexistent/autogit-test-remote")
        .unwrap();
    fs::write(repo.path.join("file"), "local change").unwrap();
    let mut daemon = Daemon::start(directory.path(), &config(vec![repo.clone()]), None);
    wait_until(|| !daemon.status().details.is_empty());
    assert!(daemon.status().details[0].error.is_some());
    assert!(daemon.status().details[0].committed);
    assert_eq!(
        daemon.request(Command::Trigger).status,
        ResponseStatus::Error
    );
    Config::update(&daemon.config_path, |config| {
        config.repositories.clear();
        Ok(())
    })
    .unwrap();
    wait_until(|| daemon.status().repositories == 0);
    assert!(daemon.status().details.is_empty());
    daemon.stop();
}
