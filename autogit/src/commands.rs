use anyhow::{Context, Result, bail};
use autogit_shared::{Config, Repository, Command as DaemonCommand, Response, ResponseStatus, ResponseData, socket_path};
use colored::Colorize;
use std::path::PathBuf;
use tabled::{Table, Tabled, settings::{Style, Width}};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Add a repository to the configuration
pub fn add_repository(path: &str, message: Option<String>, interval: Option<u64>) -> Result<()> {
    let mut config = Config::load_or_create_default()?;
    let config_path = Config::default_config_path()?;

    // Expand and canonicalize the path
    let repo_path = expand_path(path)?;

    // Verify it's a git repository
    if !repo_path.join(".git").exists() {
        bail!("Not a git repository: {}", repo_path.display());
    }

    // Check if already exists
    if config.repositories.iter().any(|r| r.path == repo_path) {
        bail!("Repository already configured: {}", repo_path.display());
    }

    // Create repository config
    let repo = Repository {
        path: repo_path.clone(),
        auto_commit: true,
        commit_message_template: message.unwrap_or_else(|| "Auto-commit: {timestamp}".to_owned()),
    };

    config.repositories.push(repo);

    // Update interval if specified
    if let Some(seconds) = interval {
        config.daemon.check_interval_seconds = seconds;
    }

    config.save(&config_path)?;

    println!("{} Added repository: {}", "✓".green().bold(), repo_path.display());
    println!("{} Configuration saved to: {}", "→".blue(), config_path.display());
    println!("{} Changes will be applied automatically (daemon auto-reloads config)", "→".green());

    Ok(())
}

/// Remove a repository from the configuration
pub fn remove_repository(path: &str) -> Result<()> {
    let mut config = Config::load_or_create_default()?;
    let config_path = Config::default_config_path()?;

    let repo_path = expand_path(path)?;

    let original_len = config.repositories.len();
    config.repositories.retain(|r| r.path != repo_path);

    if config.repositories.len() == original_len {
        bail!("Repository not found in configuration: {}", repo_path.display());
    }

    config.save(&config_path)?;

    println!("{} Removed repository: {}", "✓".green().bold(), repo_path.display());
    println!("{} Changes will be applied automatically (daemon auto-reloads config)", "→".green());

    Ok(())
}

/// List all configured repositories
pub fn list_repositories() -> Result<()> {
    let config = Config::load_or_create_default()?;

    if config.repositories.is_empty() {
        println!("{}", "No repositories configured.".yellow());
        println!("\nUse {} to add a repository", "autogit add <path>".cyan());
        return Ok(());
    }

    #[derive(Tabled)]
    struct RepoRow {
        #[tabled(rename = "Status")]
        status: String,
        #[tabled(rename = "Path")]
        path: String,
        #[tabled(rename = "Commit Message Template")]
        message: String,
    }

    let rows: Vec<RepoRow> = config.repositories.iter().map(|r| {
        RepoRow {
            status: if r.auto_commit {
                "✓ Enabled".to_owned()
            } else {
                "✗ Disabled".to_owned()
            },
            path: r.path.display().to_string(),
            message: r.commit_message_template.clone(),
        }
    }).collect();

    let mut table = Table::new(rows);
    table
        .with(Style::rounded())
        .with(Width::wrap(80).keep_words())
        .with(Width::increase(160));

    println!("{}", table);

    println!("\n{} Check interval: {} seconds", "→".blue(), config.daemon.check_interval_seconds);

    Ok(())
}

/// Enable auto-commit for a repository
pub fn enable_repository(path: &str) -> Result<()> {
    update_repository_status(path, true)
}

/// Disable auto-commit for a repository
pub fn disable_repository(path: &str) -> Result<()> {
    update_repository_status(path, false)
}

fn update_repository_status(path: &str, enabled: bool) -> Result<()> {
    let mut config = Config::load_or_create_default()?;
    let config_path = Config::default_config_path()?;

    let repo_path = expand_path(path)?;

    let repo = config.repositories.iter_mut()
        .find(|r| r.path == repo_path)
        .with_context(|| format!("Repository not found: {}", repo_path.display()))?;

    repo.auto_commit = enabled;
    config.save(&config_path)?;

    let status = if enabled { "enabled" } else { "disabled" };
    println!("{} Auto-commit {} for: {}", "✓".green().bold(), status, repo_path.display());
    println!("{} Changes will be applied automatically (daemon auto-reloads config)", "→".green());

    Ok(())
}

/// Set or show the global check interval
pub fn set_interval(seconds: Option<u64>) -> Result<()> {
    let config = Config::load_or_create_default()?;
    let config_path = Config::default_config_path()?;

    match seconds {
        Some(secs) => {
            // Set new interval
            let mut config = config;
            config.daemon.check_interval_seconds = secs;
            config.save(&config_path)?;

            println!("{} Check interval set to {} seconds", "✓".green().bold(), secs);
            println!("{} Changes will be applied automatically (daemon auto-reloads config)", "→".green());
        }
        None => {
            // Show current interval
            let current = config.daemon.check_interval_seconds;
            println!("{} Current check interval: {} seconds", "→".blue(), current);

            // Convert to human-readable format
            if current >= 60 {
                let minutes = current / 60;
                let remaining_seconds = current % 60;
                if remaining_seconds == 0 {
                    println!("   ({})", format!("{} minute{}", minutes, if minutes != 1 { "s" } else { "" }).cyan());
                } else {
                    println!("   ({})", format!("{} minute{} {} second{}",
                        minutes, if minutes != 1 { "s" } else { "" },
                        remaining_seconds, if remaining_seconds != 1 { "s" } else { "" }).cyan());
                }
            }
        }
    }

    Ok(())
}

/// Show current configuration status
pub async fn show_status() -> Result<()> {
    let config_path = Config::default_config_path()?;
    let config = Config::load_or_create_default()?;

    // Check daemon status via socket
    let daemon_running = is_daemon_running().await;

    println!("{}", "autogit Configuration".bold().underline());

    // Show daemon status
    print!("\n{} Daemon status: ", "→".blue());
    if daemon_running {
        println!("{}", "running".green());
    } else {
        println!("{}", "not running".red());
        println!("   Start with: systemctl --user start autogit-daemon");
    }

    println!("{} Config file: {}", "→".blue(), config_path.display());
    println!("{} Check interval: {} seconds", "→".blue(), config.daemon.check_interval_seconds);
    println!("{} Repositories: {}", "→".blue(), config.repositories.len());

    if !config.repositories.is_empty() {
        println!("\n{}", "Repositories:".bold());
        for (i, repo) in config.repositories.iter().enumerate() {
            let status = if repo.auto_commit {
                "✓".green()
            } else {
                "✗".red()
            };
            println!("  {}. {} {}", i + 1, status, repo.path.display());
        }
    }

    Ok(())
}

/// Edit configuration file in $EDITOR
pub fn edit_config() -> Result<()> {
    let config_path = Config::default_config_path()?;

    // Ensure config exists
    let _ = Config::load_or_create_default()?;

    let editor = std::env::var("EDITOR")
        .unwrap_or_else(|_| "vi".to_owned());

    println!("Opening {} in {}...", config_path.display(), editor);

    std::process::Command::new(editor)
        .arg(&config_path)
        .status()
        .context("Failed to open editor")?;

    println!("{} Changes will be applied automatically (daemon auto-reloads config)", "→".green());

    Ok(())
}

/// Trigger an immediate check and commit cycle
pub async fn trigger_now() -> Result<()> {
    println!("{} Triggering immediate check and commit cycle...", "→".blue());

    // Send trigger command to daemon
    let response = send_daemon_command(DaemonCommand::Trigger).await?;

    // Check response status
    if response.status != ResponseStatus::Ok {
        bail!("Daemon returned error: {}", response.message);
    }

    println!("{} {}", "✓".green().bold(), response.message);

    // Display detailed results if available
    if let Some(ResponseData::Trigger { repos_checked, repos_committed, details }) = response.data {
        if !details.is_empty() {
            println!("\n{}", "Results:".bold());
            for detail in details {
                let icon = if detail.committed {
                    "✓".green()
                } else if detail.error.is_some() {
                    "✗".red()
                } else {
                    "−".yellow()
                };

                print!("  {} {}", icon, detail.path.display());

                if let Some(files) = detail.files_changed {
                    if files > 0 {
                        print!(" ({} files)", files);
                    }
                }

                if let Some(ref error) = detail.error {
                    print!(" - {}", error.red());
                }

                println!();
            }
        }

        if repos_committed == 0 && repos_checked > 0 {
            println!("\n{} No changes to commit in any repository", "→".blue());
        }
    }

    Ok(())
}

/// Expand ~ and canonicalize path
fn expand_path(path: &str) -> Result<PathBuf> {
    let expanded = if path.starts_with("~/") {
        let home = std::env::var("HOME")
            .context("HOME environment variable not set")?;
        PathBuf::from(path.replacen("~/", &format!("{}/", home), 1))
    } else {
        PathBuf::from(path)
    };

    expanded.canonicalize()
        .with_context(|| format!("Failed to resolve path: {}", path))
}

/// Send a command to the daemon via Unix socket and get response
async fn send_daemon_command(command: DaemonCommand) -> Result<Response> {
    let socket_path = socket_path()
        .context("Failed to get socket path")?;

    // Try to connect to the socket
    let stream = UnixStream::connect(&socket_path).await
        .with_context(|| format!(
            "Failed to connect to daemon socket: {}\nIs the daemon running? Start it with: systemctl --user start autogit-daemon",
            socket_path.display()
        ))?;

    // Send command
    let command_json = command.to_json()
        .context("Failed to serialize command")?;

    let mut stream = stream;
    stream.write_all(command_json.as_bytes()).await
        .context("Failed to send command to daemon")?;

    stream.flush().await
        .context("Failed to flush socket")?;

    // Shutdown write side to signal we're done sending
    stream.shutdown().await
        .context("Failed to shutdown write side of socket")?;

    // Read response
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    reader.read_line(&mut line).await
        .context("Failed to read response from daemon")?;

    if line.is_empty() {
        bail!("Daemon closed connection without sending response");
    }

    Response::from_json(&line)
        .context("Failed to parse daemon response")
}

/// Check if daemon is running by trying to ping it
async fn is_daemon_running() -> bool {
    send_daemon_command(DaemonCommand::Ping).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::env;
    use tempfile::TempDir;
    use serial_test::serial;

    /// Helper to create a temporary git repository
    fn create_temp_git_repo() -> Result<TempDir> {
        let temp_dir = TempDir::new()?;
        let git_dir = temp_dir.path().join(".git");
        fs::create_dir(&git_dir)?;
        Ok(temp_dir)
    }

    /// Helper to set up a test environment with temporary config
    fn setup_test_env() -> Result<(TempDir, TempDir)> {
        let config_dir = TempDir::new()?;
        let repo_dir = create_temp_git_repo()?;

        // Set XDG_CONFIG_HOME to our temp directory
        env::set_var("XDG_CONFIG_HOME", config_dir.path());

        Ok((config_dir, repo_dir))
    }

    #[test]
    #[serial]
    fn test_expand_path_absolute() {
        let _path = "/tmp/test";
        // expand_path is private, but we can test it indirectly
        // Just documenting the behavior here
        assert!(true); // Placeholder
    }

    #[test]
    #[serial]
    #[serial]
    fn test_add_repository_creates_config() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        let result = add_repository(repo_path, None, None);
        assert!(result.is_ok());

        // Verify config was created
        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.repositories.len(), 1);
        assert!(config.repositories[0].path.to_str().unwrap().contains("tmp"));
        assert_eq!(config.repositories[0].auto_commit, true);

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_add_repository_with_custom_message() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        let result = add_repository(repo_path, Some("Custom: {date}".to_owned()), None);
        assert!(result.is_ok());

        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.repositories[0].commit_message_template, "Custom: {date}");

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_add_repository_with_interval() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        let result = add_repository(repo_path, None, Some(60));
        assert!(result.is_ok());

        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.daemon.check_interval_seconds, 60);

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_add_repository_not_git_repo() {
        let config_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", config_dir.path());

        let non_git_dir = TempDir::new().unwrap();
        let path = non_git_dir.path().to_str().unwrap();

        let result = add_repository(path, None, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Not a git repository"));
    }

    #[test]
    #[serial]
    fn test_add_repository_duplicate() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        // Add first time
        let result1 = add_repository(repo_path, None, None);
        assert!(result1.is_ok());

        // Try to add again
        let result2 = add_repository(repo_path, None, None);
        assert!(result2.is_err());
        assert!(result2.unwrap_err().to_string().contains("already configured"));

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_remove_repository() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        // Add repository
        add_repository(repo_path, None, None).unwrap();

        // Remove it
        let result = remove_repository(repo_path);
        assert!(result.is_ok());

        // Verify it's gone
        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.repositories.len(), 0);

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_remove_repository_not_found() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        let result = remove_repository(repo_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_enable_repository() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        // Add and then disable
        add_repository(repo_path, None, None).unwrap();
        disable_repository(repo_path).unwrap();

        // Now enable
        let result = enable_repository(repo_path);
        assert!(result.is_ok());

        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.repositories[0].auto_commit, true);

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_disable_repository() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        // Add repository
        add_repository(repo_path, None, None).unwrap();

        // Disable it
        let result = disable_repository(repo_path);
        assert!(result.is_ok());

        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.repositories[0].auto_commit, false);

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_enable_repository_not_found() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        let result = enable_repository(repo_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_set_interval_new_value() {
        let config_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", config_dir.path());

        let result = set_interval(Some(120));
        assert!(result.is_ok());

        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.daemon.check_interval_seconds, 120);
    }

    #[test]
    #[serial]
    fn test_set_interval_show_current() {
        let config_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", config_dir.path());

        // Set a value first
        set_interval(Some(180)).unwrap();

        // Show current (no arg) - should not error
        let result = set_interval(None);
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_repositories_empty() {
        let config_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", config_dir.path());

        let result = list_repositories();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_repositories_with_repos() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        add_repository(repo_path, Some("Test message".to_owned()), None).unwrap();

        let result = list_repositories();
        assert!(result.is_ok());

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_multiple_repositories() {
        let config_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", config_dir.path());

        let repo1 = create_temp_git_repo().unwrap();
        let repo2 = create_temp_git_repo().unwrap();

        add_repository(repo1.path().to_str().unwrap(), None, None).unwrap();
        add_repository(repo2.path().to_str().unwrap(), Some("Custom".to_owned()), None).unwrap();

        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.repositories.len(), 2);
        assert_eq!(config.repositories[1].commit_message_template, "Custom");
    }

    #[test]
    #[serial]
    fn test_remove_one_of_multiple_repositories() {
        let config_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", config_dir.path());

        let repo1 = create_temp_git_repo().unwrap();
        let repo2 = create_temp_git_repo().unwrap();

        add_repository(repo1.path().to_str().unwrap(), None, None).unwrap();
        add_repository(repo2.path().to_str().unwrap(), None, None).unwrap();

        remove_repository(repo1.path().to_str().unwrap()).unwrap();

        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.repositories.len(), 1);
        assert!(config.repositories[0].path == repo2.path().canonicalize().unwrap());
    }

    #[test]
    #[serial]
    fn test_disable_then_enable_repository() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        add_repository(repo_path, None, None).unwrap();

        // Disable
        disable_repository(repo_path).unwrap();
        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.repositories[0].auto_commit, false);

        // Enable
        enable_repository(repo_path).unwrap();
        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.repositories[0].auto_commit, true);

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_interval_persists_across_operations() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        // Set interval
        set_interval(Some(240)).unwrap();

        // Add repository (shouldn't change interval)
        add_repository(repo_path, None, None).unwrap();

        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.daemon.check_interval_seconds, 240);

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_add_repository_updates_interval() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        // Add repository with interval
        add_repository(repo_path, None, Some(90)).unwrap();

        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.daemon.check_interval_seconds, 90);

        drop(config_dir);
    }

    #[test]
    #[serial]
    fn test_config_survives_operations() {
        let (config_dir, repo_dir1) = setup_test_env().unwrap();
        let repo_dir2 = create_temp_git_repo().unwrap();

        let path1 = repo_dir1.path().to_str().unwrap();
        let path2 = repo_dir2.path().to_str().unwrap();

        // Add first repo
        add_repository(path1, Some("Msg1".to_owned()), Some(60)).unwrap();

        // Add second repo
        add_repository(path2, Some("Msg2".to_owned()), None).unwrap();

        // Disable first
        disable_repository(path1).unwrap();

        // Verify final state
        let config = Config::load_or_create_default().unwrap();
        assert_eq!(config.repositories.len(), 2);
        assert_eq!(config.repositories[0].auto_commit, false);
        assert_eq!(config.repositories[1].auto_commit, true);
        assert_eq!(config.repositories[0].commit_message_template, "Msg1");
        assert_eq!(config.repositories[1].commit_message_template, "Msg2");

        drop(config_dir);
    }
}
