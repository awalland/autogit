use anyhow::{Context, Result, bail};
use autogit_shared::Repository;
use git2::{Repository as GitRepository, Signature, IndexAddOption, Status, StatusOptions};
use chrono::Local;
use notify_rust::Notification;
use std::path::Path;
use std::process::Command;
use tracing::{debug, info, warn};

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

/// Push commits to remote
/// Returns true if push was successful, false if skipped or failed
fn push_changes(repo: &GitRepository, repo_path: &std::path::Path) -> Result<bool> {
    // Check if remote exists
    let has_remote = match repo.find_remote("origin") {
        Ok(_) => true,
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            debug!("No remote 'origin' configured for {}, skipping push", repo_path.display());
            return Ok(false);
        }
        Err(e) => return Err(e.into()),
    };

    if !has_remote {
        return Ok(false);
    }

    // Check if there are unpushed commits by comparing local branch with its upstream
    let has_unpushed_commits = match repo.head() {
        Ok(head) => {
            if let Some(branch_name) = head.shorthand() {
                // Try to find the upstream branch
                match repo.find_branch(branch_name, git2::BranchType::Local) {
                    Ok(branch) => {
                        match branch.upstream() {
                            Ok(upstream) => {
                                // Compare local and upstream commits
                                let local_oid = head.target();
                                let upstream_oid = upstream.get().target();
                                local_oid != upstream_oid
                            }
                            Err(_) => {
                                // No upstream configured, assume we need to push
                                true
                            }
                        }
                    }
                    Err(_) => true,
                }
            } else {
                // Detached HEAD or other unusual state
                true
            }
        }
        Err(_) => {
            // If we can't determine, assume there might be changes
            true
        }
    };

    if !has_unpushed_commits {
        debug!("Nothing to push for: {}", repo_path.display());
        return Ok(true);
    }

    // Run git push using Command
    debug!("Pushing changes for: {}", repo_path.display());

    let output = Command::new("git")
        .arg("push")
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("Failed to execute git push for {}", repo_path.display()))?;

    if output.status.success() {
        info!("Successfully pushed changes: {}", repo_path.display());
        Ok(true)
    } else {
        // Push failed - log but continue (non-fatal)
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("Push failed for {}: {}", repo_path.display(), stderr.trim());

        // Send desktop notification
        let _ = Notification::new()
            .summary("Git Push Failed")
            .body(&format!("Repository: {}\n\nError:\n{}", repo_path.display(), stderr.trim()))
            .appname(env!("CARGO_PKG_NAME"))
            .show();

        Ok(false)
    }
}

/// Pull and rebase from remote
/// Returns true if pull was successful, false if skipped or failed
fn pull_rebase(repo: &GitRepository, repo_path: &std::path::Path) -> Result<bool> {
    // Check if remote exists
    let has_remote = match repo.find_remote("origin") {
        Ok(_) => true,
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            warn!("No remote 'origin' configured for {}, skipping pull", repo_path.display());
            return Ok(false);
        }
        Err(e) => return Err(e.into()),
    };

    if !has_remote {
        return Ok(false);
    }

    // Get the current HEAD commit before pulling
    let head_before = repo.head().ok().and_then(|h| h.target());

    // Run git pull --rebase using Command
    debug!("Pulling changes for: {}", repo_path.display());

    let output = Command::new("git")
        .arg("pull")
        .arg("--rebase")
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("Failed to execute git pull for {}", repo_path.display()))?;

    if output.status.success() {
        // Re-open the repository to get the updated HEAD
        let repo_after = GitRepository::open(repo_path)?;
        let head_after = repo_after.head().ok().and_then(|h| h.target());

        if head_before != head_after && head_after.is_some() {
            info!("Successfully pulled and rebased: {}", repo_path.display());
        } else {
            debug!("Repository already up to date: {}", repo_path.display());
        }
        Ok(true)
    } else {
        // Pull failed - try to abort rebase to clean up
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("Pull --rebase failed for {}: {}", repo_path.display(), stderr.trim());

        // Send desktop notification
        let _ = Notification::new()
            .summary("Git Pull Failed")
            .body(&format!("Repository: {}\n\nError:\n{}", repo_path.display(), stderr.trim()))
            .appname(env!("CARGO_PKG_NAME"))
            .show();

        // Try to abort the rebase to leave repo in clean state
        let abort_result = Command::new("git")
            .arg("rebase")
            .arg("--abort")
            .current_dir(repo_path)
            .output();

        match abort_result {
            Ok(abort_output) if abort_output.status.success() => {
                debug!("Aborted rebase for {}", repo_path.display());
            }
            Ok(abort_output) => {
                let abort_stderr = String::from_utf8_lossy(&abort_output.stderr);
                debug!("Rebase abort produced output for {}: {}",
                       repo_path.display(), abort_stderr.trim());
            }
            Err(e) => {
                warn!("Failed to abort rebase for {}: {}", repo_path.display(), e);
            }
        }

        // Return Ok(false) to indicate we should continue (non-fatal error)
        Ok(false)
    }
}

/// Initialize a repository on daemon startup
/// Commits any pending changes and pulls from remote
pub async fn initialize_repository(repo_config: &Repository) -> Result<()> {
    tokio::task::spawn_blocking({
        let repo_config = repo_config.clone();
        move || initialize_repository_sync(&repo_config)
    })
    .await
    .context("Task panicked")??;

    Ok(())
}

fn initialize_repository_sync(repo_config: &Repository) -> Result<()> {
    let repo = open_repository(&repo_config.path)?;

    info!("Initializing repository: {}", repo_config.path.display());

    // Check if there are uncommitted changes
    if has_changes(&repo)? {
        info!("Found uncommitted changes in {}, committing before pull", repo_config.path.display());

        // Stage all changes
        stage_all_changes(&repo)?;

        // Check if there are actually staged changes
        if has_staged_changes(&repo)? {
            // Create a startup commit
            let commit_message = "Auto-commit on daemon startup";
            create_commit(&repo, commit_message)?;
            info!("Committed pending changes in {}", repo_config.path.display());

            // Push the commit
            push_changes(&repo, &repo_config.path)?;
        }
    }

    // Try to pull and rebase
    pull_rebase(&repo, &repo_config.path)?;

    Ok(())
}

/// Check if a repository has changes and commit them if needed
/// Returns true if changes were committed, false otherwise
pub async fn check_and_commit(repo_config: &Repository) -> Result<bool> {
    // Run blocking git operations in a blocking task
    let committed = tokio::task::spawn_blocking({
        let repo_config = repo_config.clone();
        move || check_and_commit_sync(&repo_config)
    })
    .await
    .context("Task panicked")??;

    Ok(committed)
}

fn check_and_commit_sync(repo_config: &Repository) -> Result<bool> {
    let repo = open_repository(&repo_config.path)?;

    // Check if there are any changes first
    let has_local_changes = has_changes(&repo)?;

    let mut committed = false;

    // Commit local changes before pulling to avoid conflicts
    if has_local_changes {
        // Stage all changes
        stage_all_changes(&repo)?;

        // Check again after staging (in case everything was already staged)
        if has_staged_changes(&repo)? {
            // Create commit
            let commit_message = format_commit_message(&repo_config.commit_message_template);
            create_commit(&repo, &commit_message)?;
            info!("Committed changes in {}: {}", repo_config.path.display(), commit_message);
            committed = true;

            // Push the commit
            push_changes(&repo, &repo_config.path)?;
        }
    }

    // Now pull and rebase (working directory is clean)
    let _ = pull_rebase(&repo, &repo_config.path)?;

    Ok(committed)
}

/// Check if repository has any changes (staged or unstaged)
fn has_changes(repo: &GitRepository) -> Result<bool> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true);
    opts.include_ignored(false);

    let statuses = repo.statuses(Some(&mut opts))
        .context("Failed to get repository status")?;

    Ok(!statuses.is_empty())
}

/// Check if repository has staged changes
fn has_staged_changes(repo: &GitRepository) -> Result<bool> {
    let mut opts = StatusOptions::new();

    let statuses = repo.statuses(Some(&mut opts))
        .context("Failed to get repository status")?;

    for entry in statuses.iter() {
        let status = entry.status();
        if status.intersects(
            Status::INDEX_NEW
                | Status::INDEX_MODIFIED
                | Status::INDEX_DELETED
                | Status::INDEX_RENAMED
                | Status::INDEX_TYPECHANGE
        ) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Stage all changes in the repository
fn stage_all_changes(repo: &GitRepository) -> Result<()> {
    let mut index = repo.index()
        .context("Failed to get repository index")?;

    // Add all files (respects .gitignore)
    index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .context("Failed to add files to index")?;

    // Also update tracked files that were deleted
    index.update_all(["*"].iter(), None)
        .context("Failed to update index")?;

    index.write()
        .context("Failed to write index")?;

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
    let config = repo.config()
        .context("Failed to get repository config")?;

    let name = config.get_string("user.name")
        .context("user.name not set in git config")?;

    let email = config.get_string("user.email")
        .context("user.email not set in git config")?;

    Signature::now(&name, &email)
        .context("Failed to create signature")
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
        assert!(message.len() == 19); // "2025-11-15 20:30:45" format
        assert!(message.contains('-'));
        assert!(message.contains(':'));
        assert!(message.contains(' '));
    }

    #[test]
    fn test_format_date_placeholder() {
        let template = "{date}";
        let message = format_commit_message(template);

        // Should match format: YYYY-MM-DD
        assert!(message.len() == 10);
        assert_eq!(message.chars().filter(|&c| c == '-').count(), 2);
    }

    #[test]
    fn test_format_time_placeholder() {
        let template = "{time}";
        let message = format_commit_message(template);

        // Should match format: HH:MM:SS
        assert!(message.len() == 8);
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
        assert!(message1.len() == message2.len());
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
}
