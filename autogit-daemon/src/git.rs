use anyhow::{Context, Result, bail};
use autogit_shared::Repository;
use chrono::Local;
use git2::{
    IndexAddOption, Repository as GitRepository, RepositoryState, Signature, Status, StatusOptions,
};
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Open a git repository with user-friendly error messages
fn open_repository(path: &Path) -> Result<GitRepository> {
    match GitRepository::open(path) {
        Ok(repo) => Ok(repo),
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            bail!(
                "Not a git repository: {}\n\
                 The .git directory may have been deleted or the path is incorrect.\n\
                 Use 'autogit remove {}' to remove it from configuration.",
                path.display(),
                path.display()
            );
        }
        Err(e) => {
            bail!("Failed to open repository {}: {}", path.display(), e);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationOutcome {
    Succeeded,
    Skipped,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub committed: bool,
    pub files_changed: Option<usize>,
    pub pull: OperationOutcome,
    pub push: OperationOutcome,
}

impl SyncOutcome {
    pub fn error_message(&self) -> Option<String> {
        let errors: Vec<_> = [("pull", &self.pull), ("push", &self.push)]
            .into_iter()
            .filter_map(|(operation, outcome)| match outcome {
                OperationOutcome::Failed(message) => Some(format!("{operation}: {message}")),
                _ => None,
            })
            .collect();
        (!errors.is_empty()).then(|| errors.join("; "))
    }
}

const GIT_TIMEOUT: Duration = Duration::from_secs(60);

fn ensure_available(repo: &GitRepository, cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        bail!("Repository synchronisation cancelled");
    }
    if repo.state() != RepositoryState::Clean || repo.index()?.has_conflicts() {
        bail!(
            "Repository has an active Git operation or unresolved conflicts; leaving it untouched"
        );
    }
    Ok(())
}

// A separate process group lets shutdown terminate Git's SSH/credential-helper children too.
fn terminate_process(child: &mut Child) -> Result<()> {
    let process_group = i32::try_from(child.id()).context("Git process ID is out of range")?;
    // Give Git's signal handlers time to remove locks before forcibly terminating
    // any remaining helpers. Never remove lock files whose ownership is unknown.
    // SAFETY: the child was placed in its own process group before spawning.
    unsafe { libc::kill(-process_group, libc::SIGTERM) };
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(250) {
        let leader_exited = child.try_wait()?.is_some();
        // SAFETY: signal zero only tests whether this command's group still exists.
        let group_exists = unsafe { libc::kill(-process_group, 0) } == 0;
        if leader_exited && !group_exists {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: any remaining members belong to the command's process group.
    unsafe { libc::kill(-process_group, libc::SIGKILL) };
    let _ = child.kill();
    child.wait().context("Failed to reap Git process")?;
    Ok(())
}

const OUTPUT_CAPTURE_LIMIT: usize = 64 * 1024;
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(250);

fn make_nonblocking(stream: &impl AsRawFd) -> std::io::Result<()> {
    let fd = stream.as_raw_fd();
    // SAFETY: the stream owns a valid descriptor for the duration of both calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn drain_output(stream: &mut impl Read, captured: &mut Vec<u8>) -> std::io::Result<bool> {
    let mut buffer = [0; 8192];
    // Limit each pass so continuous output cannot starve cancellation or deadlines.
    for _ in 0..64 {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                let remaining = OUTPUT_CAPTURE_LIMIT - captured.len();
                captured.extend_from_slice(&buffer[..count.min(remaining)]);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn run_command(mut command: Command, cancelled: &AtomicBool, timeout: Duration) -> Result<Output> {
    if cancelled.load(Ordering::Relaxed) {
        bail!("Git operation cancelled");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().context("Failed to start Git command")?;
    let result = (|| {
        let mut stdout = child.stdout.take().context("Missing Git stdout")?;
        let mut stderr = child.stderr.take().context("Missing Git stderr")?;
        make_nonblocking(&stdout)?;
        make_nonblocking(&stderr)?;
        let mut captured_stdout = Vec::new();
        let mut captured_stderr = Vec::new();
        let mut status = None;
        let mut exited_at = None;
        let started = Instant::now();
        loop {
            if cancelled.load(Ordering::Relaxed) {
                bail!("Git operation cancelled");
            }
            if started.elapsed() >= timeout {
                bail!(
                    "Git operation timed out after {} seconds",
                    timeout.as_secs()
                );
            }
            let stdout_closed = drain_output(&mut stdout, &mut captured_stdout)?;
            let stderr_closed = drain_output(&mut stderr, &mut captured_stderr)?;
            if status.is_none() {
                status = child.try_wait()?;
                if status.is_some() {
                    exited_at = Some(Instant::now());
                    let process_group =
                        i32::try_from(child.id()).context("Git process ID is out of range")?;
                    // SAFETY: this group was created for the command. Helpers that
                    // escape it cannot be killed reliably; their pipes are closed below.
                    unsafe { libc::kill(-process_group, libc::SIGKILL) };
                }
            }
            if let Some(status) = status
                && ((stdout_closed && stderr_closed)
                    || exited_at.is_some_and(|time| time.elapsed() >= OUTPUT_DRAIN_GRACE))
            {
                return Ok(Output {
                    status,
                    stdout: captured_stdout,
                    stderr: captured_stderr,
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        terminate_process(&mut child)?;
    }
    result
}

fn run_git(repo_path: &Path, arguments: &[&str], cancelled: &AtomicBool) -> OperationOutcome {
    let mut command = Command::new("git");
    command
        .args(arguments)
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0");
    match run_command(command, cancelled, GIT_TIMEOUT) {
        Ok(output) if output.status.success() => OperationOutcome::Succeeded,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = if stderr.trim().is_empty() {
                format!("git {} exited with {}", arguments.join(" "), output.status)
            } else {
                stderr.trim().to_owned()
            };
            warn!(
                "Git {} failed for {}: {}",
                arguments.join(" "),
                repo_path.display(),
                message
            );
            OperationOutcome::Failed(message)
        }
        Err(error) => OperationOutcome::Failed(format!("{error:#}")),
    }
}

fn has_origin(repo: &GitRepository) -> Result<bool> {
    match repo.find_remote("origin") {
        Ok(_) => Ok(true),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn push_changes(
    repo: &GitRepository,
    repo_path: &Path,
    cancelled: &AtomicBool,
) -> Result<OperationOutcome> {
    ensure_available(repo, cancelled)?;
    if !has_origin(repo)? {
        return Ok(OperationOutcome::Skipped);
    }
    // Always retry unpublished commits, including commits made by an earlier cycle.
    Ok(run_git(repo_path, &["push"], cancelled))
}

fn pull_rebase(
    repo: &GitRepository,
    repo_path: &Path,
    cancelled: &AtomicBool,
) -> Result<OperationOutcome> {
    ensure_available(repo, cancelled)?;
    if !has_origin(repo)? {
        return Ok(OperationOutcome::Skipped);
    }
    // Another Git client can begin an operation after the precondition check.
    // Git exposes no ownership token for a rebase, so leave failed operations for
    // manual resolution rather than risking aborting the user's work.
    Ok(run_git(repo_path, &["pull", "--rebase"], cancelled))
}

pub async fn sync_repository(
    repo_config: &Repository,
    cancelled: Arc<AtomicBool>,
) -> Result<SyncOutcome> {
    let repo_config = repo_config.clone();
    tokio::task::spawn_blocking(move || sync_repository_sync(&repo_config, &cancelled))
        .await
        .context("Git task panicked")?
}

fn sync_repository_sync(repo_config: &Repository, cancelled: &AtomicBool) -> Result<SyncOutcome> {
    let repo = open_repository(&repo_config.path)?;
    ensure_available(&repo, cancelled)?;
    info!("Synchronising repository: {}", repo_config.path.display());
    let mut outcome = SyncOutcome {
        committed: false,
        files_changed: None,
        pull: OperationOutcome::Skipped,
        push: OperationOutcome::Skipped,
    };
    if has_changes(&repo)? {
        ensure_available(&repo, cancelled)?;
        stage_all_changes(&repo)?;
        if has_staged_changes(&repo)? {
            ensure_available(&repo, cancelled)?;
            outcome.files_changed = Some(repo.statuses(None)?.len());
            let message = format_commit_message(&repo_config.commit_message_template);
            create_commit(&repo, &message)?;
            outcome.committed = true;
            info!(
                "Committed changes in {}: {}",
                repo_config.path.display(),
                message
            );
        }
    }
    // Integrate remote commits first so a rejected push can recover in this cycle.
    outcome.pull = pull_rebase(&repo, &repo_config.path, cancelled)
        .unwrap_or_else(|error| OperationOutcome::Failed(format!("{error:#}")));
    if !matches!(outcome.pull, OperationOutcome::Failed(_)) {
        outcome.push = open_repository(&repo_config.path)
            .and_then(|repo_after| push_changes(&repo_after, &repo_config.path, cancelled))
            .unwrap_or_else(|error| OperationOutcome::Failed(format!("{error:#}")));
    }
    Ok(outcome)
}

/// Check if repository has any changes (staged or unstaged)
fn has_changes(repo: &GitRepository) -> Result<bool> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true);
    opts.include_ignored(false);

    let statuses = repo
        .statuses(Some(&mut opts))
        .context("Failed to get repository status")?;

    Ok(!statuses.is_empty())
}

/// Check if repository has staged changes
fn has_staged_changes(repo: &GitRepository) -> Result<bool> {
    let mut opts = StatusOptions::new();

    let statuses = repo
        .statuses(Some(&mut opts))
        .context("Failed to get repository status")?;

    for entry in statuses.iter() {
        let status = entry.status();
        if status.intersects(
            Status::INDEX_NEW
                | Status::INDEX_MODIFIED
                | Status::INDEX_DELETED
                | Status::INDEX_RENAMED
                | Status::INDEX_TYPECHANGE,
        ) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Stage all changes in the repository
fn stage_all_changes(repo: &GitRepository) -> Result<()> {
    let mut index = repo.index().context("Failed to get repository index")?;

    // Add all files (respects .gitignore)
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .context("Failed to add files to index")?;

    // Also update tracked files that were deleted
    index
        .update_all(["*"].iter(), None)
        .context("Failed to update index")?;

    index.write().context("Failed to write index")?;

    Ok(())
}

/// Create a commit with the given message
fn create_commit(repo: &GitRepository, message: &str) -> Result<()> {
    // Get the signature from git config
    let signature = get_signature(repo)?;

    let mut index = repo.index()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;

    // Get the current HEAD commit as parent
    let parent_commit = match repo.head() {
        Ok(head) => {
            let oid = head.target().context("HEAD has no target")?;
            Some(repo.find_commit(oid)?)
        }
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => {
            // First commit in the repository
            None
        }
        Err(e) => return Err(e.into()),
    };

    // Create the commit
    let parents = parent_commit.as_ref().map(|c| vec![c]).unwrap_or_default();

    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
    )
    .context("Failed to create commit")?;

    Ok(())
}

/// Get git signature from repository config (respects .gitconfig)
fn get_signature(repo: &GitRepository) -> Result<Signature<'static>> {
    let config = repo.config().context("Failed to get repository config")?;

    let name = config
        .get_string("user.name")
        .context("user.name not set in git config")?;

    let email = config
        .get_string("user.email")
        .context("user.email not set in git config")?;

    Signature::now(&name, &email).context("Failed to create signature")
}

/// Format commit message with placeholders replaced
fn format_commit_message(template: &str) -> String {
    let now = Local::now();

    template
        .replace("{timestamp}", &now.format("%Y-%m-%d %H:%M:%S").to_string())
        .replace("{date}", &now.format("%Y-%m-%d").to_string())
        .replace("{time}", &now.format("%H:%M:%S").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_commit_message() {
        let template = "Auto-commit: {timestamp}";
        let message = format_commit_message(template);
        assert!(message.starts_with("Auto-commit: "));
        assert!(message.len() > template.len());
    }

    #[test]
    fn test_format_with_date_and_time() {
        let template = "Changes on {date} at {time}";
        let message = format_commit_message(template);
        assert!(message.contains("Changes on "));
        assert!(message.contains(" at "));
    }

    #[test]
    fn test_format_timestamp_placeholder() {
        let template = "{timestamp}";
        let message = format_commit_message(template);

        // Should match format: YYYY-MM-DD HH:MM:SS
        assert_eq!(message.len(), 19); // "2025-11-15 20:30:45" format
        assert!(message.contains('-'));
        assert!(message.contains(':'));
        assert!(message.contains(' '));
    }

    #[test]
    fn test_format_date_placeholder() {
        let template = "{date}";
        let message = format_commit_message(template);

        // Should match format: YYYY-MM-DD
        assert_eq!(message.len(), 10);
        assert_eq!(message.chars().filter(|&c| c == '-').count(), 2);
    }

    #[test]
    fn test_format_time_placeholder() {
        let template = "{time}";
        let message = format_commit_message(template);

        // Should match format: HH:MM:SS
        assert_eq!(message.len(), 8);
        assert_eq!(message.chars().filter(|&c| c == ':').count(), 2);
    }

    #[test]
    fn test_format_no_placeholders() {
        let template = "Simple commit message";
        let message = format_commit_message(template);
        assert_eq!(message, template);
    }

    #[test]
    fn test_format_multiple_same_placeholder() {
        let template = "{date} - {date}";
        let message = format_commit_message(template);

        let parts: Vec<&str> = message.split(" - ").collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], parts[1]); // Should be same date
    }

    #[test]
    fn test_format_all_placeholders() {
        let template = "ts:{timestamp} d:{date} t:{time}";
        let message = format_commit_message(template);

        assert!(message.starts_with("ts:"));
        assert!(message.contains(" d:"));
        assert!(message.contains(" t:"));
    }

    #[test]
    fn test_format_empty_template() {
        let template = "";
        let message = format_commit_message(template);
        assert_eq!(message, "");
    }

    #[test]
    fn test_format_special_characters() {
        let template = "Commit @{timestamp}!";
        let message = format_commit_message(template);
        assert!(message.starts_with("Commit @"));
        assert!(message.ends_with('!'));
    }

    #[test]
    fn test_format_unicode() {
        let template = "✓ Update {date}";
        let message = format_commit_message(template);
        assert!(message.starts_with("✓ Update "));
    }

    #[test]
    fn test_format_multiline() {
        let template = "First line\nDate: {date}\nTime: {time}";
        let message = format_commit_message(template);
        assert!(message.contains("First line\n"));
        assert!(message.contains("Date: "));
        assert!(message.contains("Time: "));
    }

    #[test]
    fn test_format_with_braces() {
        let template = "Update {{not a placeholder}} {date}";
        let message = format_commit_message(template);
        // Double braces are not placeholders
        assert!(message.contains("{{not a placeholder}}"));
    }

    #[test]
    fn test_format_partial_placeholder() {
        let template = "{dat} {timestamps}";
        let message = format_commit_message(template);
        // Partial matches should not be replaced
        assert_eq!(message, "{dat} {timestamps}");
    }

    #[test]
    fn test_format_consistency() {
        let template = "{timestamp}";
        let message1 = format_commit_message(template);

        // Wait a tiny bit to ensure time might change
        std::thread::sleep(std::time::Duration::from_millis(1));

        let message2 = format_commit_message(template);

        // Messages should be close in time (format is stable)
        assert_eq!(message1.len(), message2.len());
    }

    #[test]
    fn test_format_realistic_templates() {
        let templates = vec![
            "Auto-commit: {timestamp}",
            "Journal update: {date}",
            "Daily backup {time}",
            "Checkpoint at {date} {time}",
            "WIP",
            "Update documentation",
            "{date}: Work in progress",
        ];

        for template in templates {
            let message = format_commit_message(template);
            assert!(!message.is_empty());

            // If template had placeholders, message should be different
            if template.contains('{') {
                assert_ne!(message, template);
            } else {
                assert_eq!(message, template);
            }
        }
    }

    #[test]
    fn test_format_date_format() {
        let template = "{date}";
        let message = format_commit_message(template);

        // Verify it's a valid date format YYYY-MM-DD
        let parts: Vec<&str> = message.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 4); // Year
        assert_eq!(parts[1].len(), 2); // Month
        assert_eq!(parts[2].len(), 2); // Day
    }

    #[test]
    fn test_format_time_format() {
        let template = "{time}";
        let message = format_commit_message(template);

        // Verify it's a valid time format HH:MM:SS
        let parts: Vec<&str> = message.split(':').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 2); // Hour
        assert_eq!(parts[1].len(), 2); // Minute
        assert_eq!(parts[2].len(), 2); // Second
    }

    #[test]
    fn test_format_timestamp_format() {
        let template = "{timestamp}";
        let message = format_commit_message(template);

        // Verify format: YYYY-MM-DD HH:MM:SS
        let parts: Vec<&str> = message.split(' ').collect();
        assert_eq!(parts.len(), 2);

        // Date part
        let date_parts: Vec<&str> = parts[0].split('-').collect();
        assert_eq!(date_parts.len(), 3);

        // Time part
        let time_parts: Vec<&str> = parts[1].split(':').collect();
        assert_eq!(time_parts.len(), 3);
    }

    #[test]
    fn test_format_preserves_whitespace() {
        let template = "  {date}  {time}  ";
        let message = format_commit_message(template);

        assert!(message.starts_with("  "));
        assert!(message.ends_with("  "));
        assert!(message.contains("  ")); // Multiple spaces preserved
    }

    // Helper to create a test git repository
    fn create_test_repo() -> (tempfile::TempDir, GitRepository) {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = GitRepository::init(temp_dir.path()).unwrap();

        // Configure git user for commits
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test User").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();

        (temp_dir, repo)
    }

    // Helper to create a commit in a repo
    fn create_test_commit(repo: &GitRepository, message: &str) -> git2::Oid {
        let mut index = repo.index().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();

        let signature = Signature::now("Test User", "test@example.com").unwrap();

        let parent = match repo.head() {
            Ok(head) => {
                let oid = head.target().unwrap();
                Some(repo.find_commit(oid).unwrap())
            }
            Err(_) => None,
        };

        let parents = parent.as_ref().map(|c| vec![c]).unwrap_or_default();

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )
        .unwrap()
    }

    // Helper to write a file to a repo
    fn write_test_file(repo_path: &Path, filename: &str, content: &str) {
        std::fs::write(repo_path.join(filename), content).unwrap();
    }

    #[test]
    fn test_open_repository_success() {
        let (_temp, repo) = create_test_repo();
        let result = open_repository(repo.path().parent().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn test_open_repository_not_found() {
        let temp_dir = tempfile::tempdir().unwrap();
        let result = open_repository(temp_dir.path());
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("Not a git repository"));
    }

    #[test]
    fn test_has_changes_empty_repo() {
        let (_temp, repo) = create_test_repo();
        let result = has_changes(&repo).unwrap();
        assert!(!result);
    }

    #[test]
    fn test_has_changes_with_untracked_file() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "content");

        let result = has_changes(&repo).unwrap();
        assert!(result);
    }

    #[test]
    fn test_has_changes_with_modified_file() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "initial");

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();
        create_test_commit(&repo, "Initial commit");

        // Modify the file
        write_test_file(temp.path(), "test.txt", "modified");

        let result = has_changes(&repo).unwrap();
        assert!(result);
    }

    #[test]
    fn test_has_changes_clean_repo() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "content");

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();
        create_test_commit(&repo, "Initial commit");

        let result = has_changes(&repo).unwrap();
        assert!(!result);
    }

    #[test]
    fn test_has_staged_changes_none() {
        let (_temp, repo) = create_test_repo();
        let result = has_staged_changes(&repo).unwrap();
        assert!(!result);
    }

    #[test]
    fn test_has_staged_changes_with_staged_file() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "content");

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();

        let result = has_staged_changes(&repo).unwrap();
        assert!(result);
    }

    #[test]
    fn test_stage_all_changes_new_file() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "content");

        stage_all_changes(&repo).unwrap();

        let result = has_staged_changes(&repo).unwrap();
        assert!(result);
    }

    #[test]
    fn test_stage_all_changes_multiple_files() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "file1.txt", "content1");
        write_test_file(temp.path(), "file2.txt", "content2");
        write_test_file(temp.path(), "file3.txt", "content3");

        stage_all_changes(&repo).unwrap();

        let statuses = repo.statuses(None).unwrap();
        let staged_count = statuses
            .iter()
            .filter(|e| e.status().intersects(Status::INDEX_NEW))
            .count();

        assert_eq!(staged_count, 3);
    }

    #[test]
    fn test_stage_all_changes_modified_file() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "initial");

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();
        create_test_commit(&repo, "Initial commit");

        // Modify the file
        write_test_file(temp.path(), "test.txt", "modified");

        stage_all_changes(&repo).unwrap();

        let result = has_staged_changes(&repo).unwrap();
        assert!(result);
    }

    #[test]
    fn test_create_commit_first_commit() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "content");

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();

        create_commit(&repo, "First commit").unwrap();

        let head = repo.head().unwrap();
        let commit = repo.find_commit(head.target().unwrap()).unwrap();
        assert_eq!(commit.message().unwrap(), "First commit");
    }

    #[test]
    fn test_create_commit_with_parent() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test1.txt", "content1");

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test1.txt")).unwrap();
        index.write().unwrap();
        create_commit(&repo, "First commit").unwrap();

        // Create second commit
        write_test_file(temp.path(), "test2.txt", "content2");
        index.add_path(Path::new("test2.txt")).unwrap();
        index.write().unwrap();
        create_commit(&repo, "Second commit").unwrap();

        let head = repo.head().unwrap();
        let commit = repo.find_commit(head.target().unwrap()).unwrap();
        assert_eq!(commit.message().unwrap(), "Second commit");
        assert_eq!(commit.parent_count(), 1);
    }

    #[test]
    fn test_get_signature_success() {
        let (_temp, repo) = create_test_repo();
        let signature = get_signature(&repo).unwrap();
        assert_eq!(signature.name().unwrap(), "Test User");
        assert_eq!(signature.email().unwrap(), "test@example.com");
    }

    #[test]
    fn test_get_signature_uses_repo_config() {
        let (_temp, repo) = create_test_repo();

        // We configured the repo with specific values
        let signature = get_signature(&repo).unwrap();

        // Should use the test config (not global config)
        assert_eq!(signature.name().unwrap(), "Test User");
        assert_eq!(signature.email().unwrap(), "test@example.com");
    }

    #[test]
    fn test_push_changes_no_remote() {
        let (_temp, repo) = create_test_repo();
        let result = push_changes(
            &repo,
            repo.path().parent().unwrap(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result, OperationOutcome::Skipped);
    }

    #[test]
    fn test_pull_rebase_no_remote() {
        let (_temp, repo) = create_test_repo();
        let result = pull_rebase(
            &repo,
            repo.path().parent().unwrap(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result, OperationOutcome::Skipped);
    }

    #[tokio::test]
    async fn test_sync_repository_no_changes() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "content");

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();
        create_test_commit(&repo, "Initial commit");

        let repo_config = Repository {
            path: temp.path().to_path_buf(),
            auto_commit: true,
            commit_message_template: "Auto: {timestamp}".to_owned(),
        };

        let committed = sync_repository(&repo_config, Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert!(!committed.committed);
    }

    #[tokio::test]
    async fn test_sync_repository_with_changes() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "initial");

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();
        create_test_commit(&repo, "Initial commit");

        // Make a change
        write_test_file(temp.path(), "test.txt", "modified");

        let repo_config = Repository {
            path: temp.path().to_path_buf(),
            auto_commit: true,
            commit_message_template: "Auto: {timestamp}".to_owned(),
        };

        let committed = sync_repository(&repo_config, Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert!(committed.committed);

        // Verify the commit was created
        let repo_after = GitRepository::open(temp.path()).unwrap();
        let head = repo_after.head().unwrap();
        let commit = repo_after.find_commit(head.target().unwrap()).unwrap();
        assert!(commit.message().unwrap().starts_with("Auto: "));
    }

    #[tokio::test]
    async fn test_sync_repository_with_new_file() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "initial.txt", "content");

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("initial.txt")).unwrap();
        index.write().unwrap();
        create_test_commit(&repo, "Initial commit");

        // Add a new file
        write_test_file(temp.path(), "newfile.txt", "new content");

        let repo_config = Repository {
            path: temp.path().to_path_buf(),
            auto_commit: true,
            commit_message_template: "New file added".to_owned(),
        };

        let committed = sync_repository(&repo_config, Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert!(committed.committed);

        // Verify the new file was committed
        let repo_after = GitRepository::open(temp.path()).unwrap();
        let head = repo_after.head().unwrap();
        let commit = repo_after.find_commit(head.target().unwrap()).unwrap();
        assert_eq!(commit.message().unwrap(), "New file added");
    }

    #[tokio::test]
    async fn test_sync_repository_clean() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "content");

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();
        create_test_commit(&repo, "Initial commit");

        let repo_config = Repository {
            path: temp.path().to_path_buf(),
            auto_commit: true,
            commit_message_template: "Auto: {timestamp}".to_owned(),
        };

        // Should succeed without errors
        let result = sync_repository(&repo_config, Arc::new(AtomicBool::new(false))).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_sync_repository_uses_configured_template() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "content");

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();
        create_test_commit(&repo, "Initial commit");

        // Add uncommitted change
        write_test_file(temp.path(), "test.txt", "modified");

        let repo_config = Repository {
            path: temp.path().to_path_buf(),
            auto_commit: true,
            commit_message_template: "Auto: {timestamp}".to_owned(),
        };

        let result = sync_repository(&repo_config, Arc::new(AtomicBool::new(false))).await;
        assert!(result.is_ok());

        // Verify the change was committed
        let repo_after = GitRepository::open(temp.path()).unwrap();
        let head = repo_after.head().unwrap();
        let commit = repo_after.find_commit(head.target().unwrap()).unwrap();
        assert!(commit.message().unwrap().starts_with("Auto: "));
    }

    #[tokio::test]
    async fn test_sync_repository_respects_gitignore() {
        let (temp, repo) = create_test_repo();

        // Create .gitignore
        write_test_file(temp.path(), ".gitignore", "*.log\n");

        let mut index = repo.index().unwrap();
        index.add_path(Path::new(".gitignore")).unwrap();
        index.write().unwrap();
        create_test_commit(&repo, "Add gitignore");

        // Add both tracked and ignored file
        write_test_file(temp.path(), "tracked.txt", "content");
        write_test_file(temp.path(), "ignored.log", "log content");

        let repo_config = Repository {
            path: temp.path().to_path_buf(),
            auto_commit: true,
            commit_message_template: "Auto commit".to_owned(),
        };

        let committed = sync_repository(&repo_config, Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert!(committed.committed);

        // Verify only tracked file was committed
        let repo_after = GitRepository::open(temp.path()).unwrap();
        let head = repo_after.head().unwrap();
        let commit = repo_after.find_commit(head.target().unwrap()).unwrap();
        let tree = commit.tree().unwrap();

        assert!(tree.get_name("tracked.txt").is_some());
        assert!(tree.get_name("ignored.log").is_none());
    }
    fn repo_config(path: &Path) -> Repository {
        Repository {
            path: path.to_owned(),
            auto_commit: true,
            commit_message_template: "Automatic update".to_owned(),
        }
    }

    fn git_command(path: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(path)
            .output()
            .unwrap()
    }

    fn successful_git(path: &Path, args: &[&str]) {
        let output = git_command(path, args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn conflicting_repo(operation: &str) -> (tempfile::TempDir, GitRepository) {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "initial\n");
        stage_all_changes(&repo).unwrap();
        create_test_commit(&repo, "Initial");
        let original_branch = repo.head().unwrap().shorthand().unwrap().to_owned();
        successful_git(temp.path(), &["checkout", "-b", "other"]);
        write_test_file(temp.path(), "test.txt", "other\n");
        stage_all_changes(&repo).unwrap();
        create_test_commit(&repo, "Other change");
        successful_git(temp.path(), &["checkout", &original_branch]);
        write_test_file(temp.path(), "test.txt", "local\n");
        stage_all_changes(&repo).unwrap();
        create_test_commit(&repo, "Local change");
        assert!(
            !git_command(temp.path(), &[operation, "other"])
                .status
                .success()
        );
        let repo = GitRepository::open(temp.path()).unwrap();
        assert!(repo.index().unwrap().has_conflicts());
        (temp, repo)
    }

    #[test]
    fn active_merge_and_rebase_remain_untouched() {
        for operation in ["merge", "rebase"] {
            let (temp, repo) = conflicting_repo(operation);
            let state = repo.state();
            let head = repo.head().unwrap().target();
            let index = std::fs::read(repo.path().join("index")).unwrap();
            let file = std::fs::read(temp.path().join("test.txt")).unwrap();
            let result = sync_repository_sync(&repo_config(temp.path()), &AtomicBool::new(false));
            assert!(result.is_err());
            assert_eq!(repo.state(), state);
            assert_eq!(repo.head().unwrap().target(), head);
            assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), index);
            assert_eq!(std::fs::read(temp.path().join("test.txt")).unwrap(), file);
            assert!(repo.index().unwrap().has_conflicts());
            assert!(pull_rebase(&repo, temp.path(), &AtomicBool::new(false)).is_err());
            assert_eq!(repo.state(), state);
        }
    }

    #[test]
    fn conflicts_are_rejected_even_without_operation_marker() {
        let (temp, repo) = conflicting_repo("merge");
        std::fs::remove_file(repo.path().join("MERGE_HEAD")).unwrap();
        assert_eq!(repo.state(), RepositoryState::Clean);
        let index = std::fs::read(repo.path().join("index")).unwrap();
        assert!(sync_repository_sync(&repo_config(temp.path()), &AtomicBool::new(false)).is_err());
        assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), index);
    }

    #[test]
    fn commands_timeout_and_kill_descendants_holding_output_pipes() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & wait"]);
        let started = Instant::now();
        let error =
            run_command(command, &AtomicBool::new(false), Duration::from_millis(80)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn running_commands_are_cancelled_and_reaped() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let trigger = cancelled.clone();
        let cancellation = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            trigger.store(true, Ordering::Relaxed);
        });
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & wait"]);
        let started = Instant::now();
        let error = run_command(command, &cancelled, Duration::from_secs(10)).unwrap_err();
        cancellation.join().unwrap();
        assert!(error.to_string().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn remote_fixture() -> (tempfile::TempDir, tempfile::TempDir, GitRepository) {
        let remote_dir = tempfile::tempdir().unwrap();
        GitRepository::init_bare(remote_dir.path()).unwrap();
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "initial\n");
        stage_all_changes(&repo).unwrap();
        create_test_commit(&repo, "Initial");
        repo.remote("origin", remote_dir.path().to_str().unwrap())
            .unwrap();
        successful_git(temp.path(), &["push", "-u", "origin", "HEAD"]);
        (remote_dir, temp, repo)
    }

    #[test]
    fn failed_push_is_retried_without_new_file_changes() {
        use std::os::unix::fs::PermissionsExt;
        let (remote, temp, repo) = remote_fixture();
        let hook = remote.path().join("hooks/pre-receive");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_test_file(temp.path(), "test.txt", "changed\n");
        let first =
            sync_repository_sync(&repo_config(temp.path()), &AtomicBool::new(false)).unwrap();
        assert!(first.committed);
        assert!(matches!(first.push, OperationOutcome::Failed(_)));
        assert!(first.error_message().unwrap().contains("push:"));
        std::fs::remove_file(hook).unwrap();
        let second =
            sync_repository_sync(&repo_config(temp.path()), &AtomicBool::new(false)).unwrap();
        assert!(!second.committed);
        assert_eq!(second.push, OperationOutcome::Succeeded);
        let remote_repo = GitRepository::open_bare(remote.path()).unwrap();
        let branch = repo.head().unwrap().name().unwrap().to_owned();
        assert_eq!(
            remote_repo.find_reference(&branch).unwrap().target(),
            repo.head().unwrap().target()
        );
    }

    #[test]
    fn diverged_changes_are_rebased_then_pushed_in_one_cycle() {
        let (remote, temp, repo) = remote_fixture();
        let other_dir = tempfile::tempdir().unwrap();
        let other = GitRepository::clone(temp.path().to_str().unwrap(), other_dir.path()).unwrap();
        let mut config = other.config().unwrap();
        config.set_str("user.name", "Test User").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        other
            .remote_set_url("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_test_file(other_dir.path(), "remote.txt", "remote change\n");
        stage_all_changes(&other).unwrap();
        let remote_commit = create_test_commit(&other, "Remote change");
        successful_git(other_dir.path(), &["push", "origin", "HEAD"]);
        write_test_file(temp.path(), "test.txt", "local change\n");
        let outcome =
            sync_repository_sync(&repo_config(temp.path()), &AtomicBool::new(false)).unwrap();
        assert!(outcome.committed);
        assert_eq!(outcome.pull, OperationOutcome::Succeeded);
        assert_eq!(outcome.push, OperationOutcome::Succeeded);
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.parent_id(0).unwrap(), remote_commit);
        let remote_repo = GitRepository::open_bare(remote.path()).unwrap();
        let branch = repo.head().unwrap().name().unwrap().to_owned();
        assert_eq!(
            remote_repo.find_reference(&branch).unwrap().target(),
            Some(head.id())
        );
    }

    #[test]
    fn failed_pull_is_an_explicit_failure() {
        let (temp, repo) = create_test_repo();
        repo.remote("origin", "/nonexistent/autogit-test-remote")
            .unwrap();
        write_test_file(temp.path(), "test.txt", "local change\n");
        let outcome =
            sync_repository_sync(&repo_config(temp.path()), &AtomicBool::new(false)).unwrap();
        assert!(outcome.committed);
        assert!(matches!(outcome.pull, OperationOutcome::Failed(_)));
        assert_eq!(outcome.push, OperationOutcome::Skipped);
        assert!(outcome.error_message().unwrap().contains("pull:"));
        assert_eq!(repo.state(), RepositoryState::Clean);
    }
    #[test]
    fn failed_daemon_rebase_is_preserved_for_manual_resolution() {
        let (remote, temp, _repo) = remote_fixture();
        let other_dir = tempfile::tempdir().unwrap();
        let other = GitRepository::clone(temp.path().to_str().unwrap(), other_dir.path()).unwrap();
        other
            .remote_set_url("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_test_file(other_dir.path(), "test.txt", "remote change\n");
        stage_all_changes(&other).unwrap();
        create_test_commit(&other, "Remote change");
        successful_git(other_dir.path(), &["push", "origin", "HEAD"]);
        write_test_file(temp.path(), "test.txt", "local change\n");
        let outcome =
            sync_repository_sync(&repo_config(temp.path()), &AtomicBool::new(false)).unwrap();
        assert!(outcome.committed);
        assert!(matches!(outcome.pull, OperationOutcome::Failed(_)));
        assert_eq!(outcome.push, OperationOutcome::Skipped);
        let repo = GitRepository::open(temp.path()).unwrap();
        assert_ne!(repo.state(), RepositoryState::Clean);
        assert!(repo.index().unwrap().has_conflicts());
        let content = std::fs::read_to_string(temp.path().join("test.txt")).unwrap();
        assert!(content.contains("<<<<<<<"));
        assert!(content.contains("local change"));
        assert!(content.contains("remote change"));
        let original = repo
            .find_reference("ORIG_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        assert_eq!(original.message(), Some("Automatic update"));
        let state = repo.state();
        let index = std::fs::read(repo.path().join("index")).unwrap();
        assert!(sync_repository_sync(&repo_config(temp.path()), &AtomicBool::new(false)).is_err());
        assert_eq!(repo.state(), state);
        assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), index);
    }

    #[test]
    fn failed_pull_does_not_abort_rebase_started_by_another_client() {
        use std::os::unix::fs::PermissionsExt;
        let (temp, repo) = conflicting_repo("rebase");
        successful_git(temp.path(), &["rebase", "--abort"]);
        let remote = tempfile::tempdir().unwrap();
        GitRepository::init_bare(remote.path()).unwrap();
        repo.remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        let ready = remote.path().join("ready");
        let release = remote.path().join("release");
        let helper = remote.path().join("upload-pack");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\nexit 1\n",
                ready.display(),
                release.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        repo.config()
            .unwrap()
            .set_str("remote.origin.uploadpack", helper.to_str().unwrap())
            .unwrap();
        let path = temp.path().to_owned();
        let pull = thread::spawn(move || {
            let repo = GitRepository::open(&path).unwrap();
            pull_rebase(&repo, &path, &AtomicBool::new(false)).unwrap()
        });
        let started = Instant::now();
        while !ready.exists() && started.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(10));
        }
        if !ready.exists() {
            std::fs::write(&release, "").unwrap();
            pull.join().unwrap();
            panic!("Pull did not reach the upload-pack gate");
        }
        assert!(
            !git_command(temp.path(), &["rebase", "other"])
                .status
                .success()
        );
        let state = repo.state();
        let head = repo.head().unwrap().target();
        let index = std::fs::read(repo.path().join("index")).unwrap();
        let content = std::fs::read(temp.path().join("test.txt")).unwrap();
        std::fs::write(&release, "").unwrap();
        assert!(matches!(pull.join().unwrap(), OperationOutcome::Failed(_)));
        assert_ne!(state, RepositoryState::Clean);
        assert_eq!(repo.state(), state);
        assert_eq!(repo.head().unwrap().target(), head);
        assert_eq!(std::fs::read(repo.path().join("index")).unwrap(), index);
        assert_eq!(
            std::fs::read(temp.path().join("test.txt")).unwrap(),
            content
        );
    }

    struct EscapedHelper {
        directory: tempfile::TempDir,
    }

    impl EscapedHelper {
        fn command(&self, exit_parent: bool) -> Command {
            let mut command = Command::new("sh");
            command.args([
                "-c",
                if exit_parent {
                    "setsid sh -c 'echo $$ > \"$1\"; exec sleep 30' sh \"$1\" & while [ ! -s \"$1\" ]; do sleep 0.01; done; exit 0"
                } else {
                    "setsid sh -c 'echo $$ > \"$1\"; exec sleep 30' sh \"$1\" & wait"
                },
                "sh",
            ]).arg(self.directory.path().join("pid"));
            command
        }
    }

    impl Drop for EscapedHelper {
        fn drop(&mut self) {
            if let Ok(pid) = std::fs::read_to_string(self.directory.path().join("pid")) {
                let pid: i32 = pid.trim().parse().unwrap();
                // SAFETY: the test helper records its own process ID before exec.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }

    #[test]
    fn exited_command_closes_pipes_held_by_escaped_helper() {
        let helper = EscapedHelper {
            directory: tempfile::tempdir().unwrap(),
        };
        let started = Instant::now();
        let output = run_command(
            helper.command(true),
            &AtomicBool::new(false),
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(output.status.success());
        assert!(helper.directory.path().join("pid").exists());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn timeout_closes_pipes_held_by_escaped_helper() {
        let helper = EscapedHelper {
            directory: tempfile::tempdir().unwrap(),
        };
        let started = Instant::now();
        let error = run_command(
            helper.command(false),
            &AtomicBool::new(false),
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(helper.directory.path().join("pid").exists());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cancellation_closes_pipes_held_by_escaped_helper() {
        let helper = EscapedHelper {
            directory: tempfile::tempdir().unwrap(),
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let trigger = cancelled.clone();
        let marker = helper.directory.path().join("pid");
        let cancellation = thread::spawn(move || {
            let started = Instant::now();
            while !marker.exists() && started.elapsed() < Duration::from_secs(2) {
                thread::sleep(Duration::from_millis(10));
            }
            trigger.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let error =
            run_command(helper.command(false), &cancelled, Duration::from_secs(5)).unwrap_err();
        cancellation.join().unwrap();
        assert!(error.to_string().contains("cancelled"));
        assert!(helper.directory.path().join("pid").exists());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn output_capture_is_capped_while_excess_bytes_are_drained() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2",
        ]);
        let output = run_command(command, &AtomicBool::new(false), Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), OUTPUT_CAPTURE_LIMIT);
        assert_eq!(output.stderr.len(), OUTPUT_CAPTURE_LIMIT);
    }

    #[test]
    fn exited_command_terminates_group_helpers() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & exit 0"]);
        let started = Instant::now();
        assert!(
            run_command(command, &AtomicBool::new(false), Duration::from_secs(1))
                .unwrap()
                .status
                .success()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cancellation_before_sync_preserves_working_tree_and_index() {
        let (temp, repo) = create_test_repo();
        write_test_file(temp.path(), "test.txt", "content");
        assert!(sync_repository_sync(&repo_config(temp.path()), &AtomicBool::new(true)).is_err());
        assert!(!has_staged_changes(&repo).unwrap());
        assert!(repo.head().is_err());
    }
    #[test]
    fn cancellation_during_push_preserves_committed_outcome() {
        use std::os::unix::fs::PermissionsExt;
        let (remote, temp, repo) = remote_fixture();
        let marker = remote.path().join("push-started");
        let hook = remote.path().join("hooks/pre-receive");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\nsleep 30\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_test_file(temp.path(), "test.txt", "changed\n");
        let cancelled = Arc::new(AtomicBool::new(false));
        let trigger = cancelled.clone();
        let cancellation = thread::spawn(move || {
            let started = Instant::now();
            while !marker.exists() && started.elapsed() < Duration::from_secs(5) {
                thread::sleep(Duration::from_millis(10));
            }
            trigger.store(true, Ordering::Relaxed);
        });
        let outcome = sync_repository_sync(&repo_config(temp.path()), &cancelled).unwrap();
        cancellation.join().unwrap();
        assert!(outcome.committed);
        assert!(matches!(outcome.push, OperationOutcome::Failed(_)));
        assert!(outcome.error_message().unwrap().contains("cancelled"));
        assert_eq!(repo.state(), RepositoryState::Clean);
        assert!(!has_changes(&repo).unwrap());
    }
    #[test]
    fn cancelled_command_can_clean_up_before_forced_termination() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("cleaned-up");
        let ready = directory.path().join("ready");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "trap 'touch \"$1\"; exit 0' TERM; touch \"$2\"; sleep 30 & wait",
                "sh",
            ])
            .arg(&marker)
            .arg(&ready);
        let cancelled = Arc::new(AtomicBool::new(false));
        let trigger = cancelled.clone();
        let cancellation = thread::spawn(move || {
            let started = Instant::now();
            while !ready.exists() && started.elapsed() < Duration::from_secs(2) {
                thread::sleep(Duration::from_millis(10));
            }
            trigger.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let error = run_command(command, &cancelled, Duration::from_secs(5)).unwrap_err();
        cancellation.join().unwrap();
        assert!(error.to_string().contains("cancelled"));
        assert!(marker.exists());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn termination_grace_is_bounded_when_command_ignores_sigterm() {
        let mut command = Command::new("sh");
        command.args(["-c", "trap '' TERM; sleep 30 & wait"]);
        let started = Instant::now();
        let error =
            run_command(command, &AtomicBool::new(false), Duration::from_millis(80)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
