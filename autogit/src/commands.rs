use anyhow::{Context, Result, bail};
use autogit_shared::{
    Command as DaemonCommand, Config, Repository, Response, ResponseData, ResponseStatus,
    socket_path,
};
use colored::Colorize;
use std::path::{Component, Path, PathBuf};
use tabled::{
    builder::Builder,
    settings::{Modify, Style, Width, object::Columns},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{Duration, Instant, timeout_at};

/// Add a repository to the configuration
pub fn add_repository(path: &str, message: Option<String>, interval: Option<u64>) -> Result<()> {
    add_repository_at(&Config::default_config_path()?, path, message, interval)
}

pub(crate) fn add_repository_at(
    config_path: &Path,
    path: &str,
    message: Option<String>,
    interval: Option<u64>,
) -> Result<()> {
    // Expand and canonicalize the path
    let repo_path = expand_path(path)?;

    // Verify it's a git repository
    if !repo_path.join(".git").exists() {
        bail!("Not a git repository: {}", repo_path.display());
    }

    Config::update(config_path, |config| {
        if config.repositories.iter().any(|r| r.path == repo_path) {
            bail!("Repository already configured: {}", repo_path.display());
        }
        config.repositories.push(Repository {
            path: repo_path.clone(),
            auto_commit: true,
            commit_message_template: message
                .unwrap_or_else(|| "Auto-commit: {timestamp}".to_owned()),
        });
        if let Some(seconds) = interval {
            config.daemon.check_interval_seconds = seconds;
        }
        Ok(())
    })?;

    println!(
        "{} Added repository: {}",
        "✓".green().bold(),
        repo_path.display()
    );
    println!(
        "{} Configuration saved to: {}",
        "→".blue(),
        config_path.display()
    );
    println!(
        "{} Changes will be applied automatically (daemon auto-reloads config)",
        "→".green()
    );

    Ok(())
}

/// Remove a repository from the configuration
pub fn remove_repository(path: &str) -> Result<()> {
    remove_repository_at(&Config::default_config_path()?, path)
}

pub(crate) fn remove_repository_at(config_path: &Path, path: &str) -> Result<()> {
    let repo_path = repository_identity(path)?;
    Config::update(config_path, |config| {
        let original_len = config.repositories.len();
        config.repositories.retain(|r| r.path != repo_path);
        if config.repositories.len() == original_len {
            bail!(
                "Repository not found in configuration: {}",
                repo_path.display()
            );
        }
        Ok(())
    })?;

    println!(
        "{} Removed repository: {}",
        "✓".green().bold(),
        repo_path.display()
    );
    println!(
        "{} Changes will be applied automatically (daemon auto-reloads config)",
        "→".green()
    );

    Ok(())
}

/// List all configured repositories
pub fn list_repositories() -> Result<()> {
    list_repositories_at(&Config::default_config_path()?)
}

pub(crate) fn list_repositories_at(config_path: &Path) -> Result<()> {
    let config = Config::load_or_create(config_path)?;

    if config.repositories.is_empty() {
        println!("{}", "No repositories configured.".yellow());
        println!("\nUse {} to add a repository", "autogit add <path>".cyan());
        return Ok(());
    }

    let mut builder = Builder::default();
    builder.push_record(["Status", "Path", "Commit Message Template"]);
    for repository in &config.repositories {
        builder.push_record([
            if repository.auto_commit {
                "✓ Enabled".to_owned()
            } else {
                "✗ Disabled".to_owned()
            },
            repository.path.display().to_string(),
            repository.commit_message_template.clone(),
        ]);
    }

    let mut table = builder.build();
    table
        .with(Style::rounded())
        .with(Modify::new(Columns::new(0..1)).with(Width::truncate(11).suffix("")))
        .with(Modify::new(Columns::new(1..)).with(Width::wrap(60).keep_words(true)));

    println!("{}", table);

    println!(
        "\n{} Check interval: {} seconds",
        "→".blue(),
        config.daemon.check_interval_seconds
    );

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
    update_repository_status_at(&Config::default_config_path()?, path, enabled)
}

pub(crate) fn update_repository_status_at(
    config_path: &Path,
    path: &str,
    enabled: bool,
) -> Result<()> {
    let repo_path = repository_identity(path)?;
    Config::update(config_path, |config| {
        let repo = config
            .repositories
            .iter_mut()
            .find(|r| r.path == repo_path)
            .with_context(|| format!("Repository not found: {}", repo_path.display()))?;
        repo.auto_commit = enabled;
        Ok(())
    })?;

    let status = if enabled { "enabled" } else { "disabled" };
    println!(
        "{} Auto-commit {} for: {}",
        "✓".green().bold(),
        status,
        repo_path.display()
    );
    println!(
        "{} Changes will be applied automatically (daemon auto-reloads config)",
        "→".green()
    );

    Ok(())
}

/// Set or show the global check interval
pub fn set_interval(seconds: Option<u64>) -> Result<()> {
    set_interval_at(&Config::default_config_path()?, seconds)
}

pub(crate) fn set_interval_at(config_path: &Path, seconds: Option<u64>) -> Result<()> {
    match seconds {
        Some(secs) => {
            Config::update(config_path, |config| {
                config.daemon.check_interval_seconds = secs;
                Ok(())
            })?;

            println!(
                "{} Check interval set to {} seconds",
                "✓".green().bold(),
                secs
            );
            println!(
                "{} Changes will be applied automatically (daemon auto-reloads config)",
                "→".green()
            );
        }
        None => {
            // Show current interval
            let current = Config::load_or_create(config_path)?
                .daemon
                .check_interval_seconds;
            println!("{} Current check interval: {} seconds", "→".blue(), current);

            // Convert to human-readable format
            if current >= 60 {
                let minutes = current / 60;
                let remaining_seconds = current % 60;
                if remaining_seconds == 0 {
                    println!(
                        "   ({})",
                        format!("{} minute{}", minutes, if minutes != 1 { "s" } else { "" }).cyan()
                    );
                } else {
                    println!(
                        "   ({})",
                        format!(
                            "{} minute{} {} second{}",
                            minutes,
                            if minutes != 1 { "s" } else { "" },
                            remaining_seconds,
                            if remaining_seconds != 1 { "s" } else { "" }
                        )
                        .cyan()
                    );
                }
            }
        }
    }

    Ok(())
}

/// Show current configuration status
pub async fn show_status() -> Result<()> {
    show_status_at(&Config::default_config_path()?, &socket_path()?).await
}

async fn show_status_at(config_path: &Path, daemon_socket: &Path) -> Result<()> {
    println!("{}", "autogit Status".bold().underline());
    println!("{} Config file: {}", "→".blue(), config_path.display());
    match send_daemon_command_at(daemon_socket, DaemonCommand::Status).await {
        Ok(response) => display_daemon_status(response),
        Err(error) => {
            println!("{} Daemon status: {}", "→".blue(), "unavailable".red());
            println!("   {}", error);
            if config_path.exists() {
                let config = Config::load(config_path)?;
                println!(
                    "{} Configured interval: {} seconds (daemon unavailable)",
                    "→".blue(),
                    config.daemon.check_interval_seconds
                );
                println!(
                    "{} Configured repositories: {}",
                    "→".blue(),
                    config.repositories.len()
                );
            }
            Ok(())
        }
    }
}

fn display_daemon_status(response: Response) -> Result<()> {
    if response.status != ResponseStatus::Ok {
        bail!("Daemon returned error: {}", response.message);
    }
    let Some(ResponseData::Status {
        uptime_seconds,
        check_interval_seconds,
        config,
        suspended,
        syncing,
        details,
        ..
    }) = response.data
    else {
        bail!("Daemon returned no status data");
    };
    let state = if suspended {
        "suspended"
    } else if syncing {
        "syncing"
    } else {
        "running"
    };
    println!("{} Daemon status: {}", "→".blue(), state.green());
    println!("{} Uptime: {} seconds", "→".blue(), uptime_seconds);
    println!(
        "{} Applied check interval: {} seconds",
        "→".blue(),
        check_interval_seconds
    );
    println!(
        "{} Applied repositories: {}",
        "→".blue(),
        config.repositories.len()
    );
    for repo in &config.repositories {
        let enabled = if repo.auto_commit {
            "enabled"
        } else {
            "disabled"
        };
        let result = match details.iter().find(|detail| detail.path == repo.path) {
            Some(detail) => match &detail.error {
                Some(error) => error.red().to_string(),
                None if detail.committed => "committed".to_owned(),
                None => "checked".to_owned(),
            },
            None => "not checked yet".to_owned(),
        };
        println!("  {} ({}): {}", repo.path.display(), enabled, result);
    }
    Ok(())
}

/// Edit configuration file in $EDITOR
pub fn edit_config() -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_owned());
    edit_config_at(&Config::default_config_path()?, &editor)
}

fn edit_config_at(config_path: &Path, editor: &str) -> Result<()> {
    let arguments = shell_words::split(editor).context("Failed to parse EDITOR command")?;
    let (program, arguments) = arguments.split_first().context("EDITOR command is empty")?;
    println!("Opening {} in {}...", config_path.display(), editor);
    Config::edit_file(config_path, |temporary_path| {
        let status = std::process::Command::new(program)
            .args(arguments)
            .arg(temporary_path)
            .status()
            .context("Failed to open editor")?;
        if !status.success() {
            bail!("Editor exited unsuccessfully: {}", status);
        }
        Ok(())
    })?;

    println!(
        "{} Changes will be applied automatically (daemon auto-reloads config)",
        "→".green()
    );

    Ok(())
}

/// Trigger an immediate check and commit cycle
pub async fn trigger_now() -> Result<()> {
    println!(
        "{} Triggering immediate check and commit cycle...",
        "→".blue()
    );

    // Send trigger command to daemon
    let response = send_daemon_command(DaemonCommand::Trigger).await?;
    display_trigger_result(response)
}

fn display_trigger_result(response: Response) -> Result<()> {
    let failed = response.status != ResponseStatus::Ok;
    println!("{}", response.message);
    let mut repository_failed = false;

    // Display detailed results if available
    if let Some(ResponseData::Trigger {
        repos_checked,
        repos_committed,
        details,
    }) = response.data
    {
        if !details.is_empty() {
            println!("\n{}", "Results:".bold());
            for detail in details {
                repository_failed |= detail.error.is_some();
                let icon = if detail.error.is_some() {
                    "✗".red()
                } else if detail.committed {
                    "✓".green()
                } else {
                    "−".yellow()
                };

                print!("  {} {}", icon, detail.path.display());

                if let Some(files) = detail.files_changed
                    && files > 0
                {
                    print!(" ({} files)", files);
                }

                if let Some(ref error) = detail.error {
                    print!(" - {}", error.red());
                }

                println!();
            }
        }

        if !repository_failed && !failed && repos_committed == 0 && repos_checked > 0 {
            println!("\n{} No changes to commit in any repository", "→".blue());
        }
    }

    if failed || repository_failed {
        bail!("Synchronisation failed: {}", response.message);
    }
    Ok(())
}

/// Suspend the daemon (stop automatic syncing)
pub async fn suspend_daemon() -> Result<()> {
    println!("{} Suspending daemon...", "→".blue());

    // Send suspend command to daemon
    let response = send_daemon_command(DaemonCommand::Suspend).await?;

    // Check response status
    if response.status != ResponseStatus::Ok {
        bail!("Daemon returned error: {}", response.message);
    }

    println!("{} {}", "✓".green().bold(), response.message);

    Ok(())
}

/// Resume the daemon (restart automatic syncing)
pub async fn resume_daemon() -> Result<()> {
    println!("{} Resuming daemon...", "→".blue());

    // Send resume command to daemon
    let response = send_daemon_command(DaemonCommand::Resume).await?;

    // Check response status
    if response.status != ResponseStatus::Ok {
        bail!("Daemon returned error: {}", response.message);
    }

    println!("{} {}", "✓".green().bold(), response.message);

    Ok(())
}

/// Expand ~ and canonicalize path
fn expand_path(path: &str) -> Result<PathBuf> {
    expand_user_path(path)?
        .canonicalize()
        .with_context(|| format!("Failed to resolve path: {}", path))
}

/// Resolve identity through existing ancestors even when the repository has been removed.
fn repository_identity(path: &str) -> Result<PathBuf> {
    let absolute = std::path::absolute(expand_user_path(path)?)?;
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            component => {
                resolved.push(component.as_os_str());
                match resolved.canonicalize() {
                    Ok(existing) => resolved = existing,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("Failed to resolve repository path"),
                }
            }
        }
    }
    Ok(resolved)
}

fn expand_user_path(path: &str) -> Result<PathBuf> {
    if let Some(relative) = path.strip_prefix("~/") {
        let home = std::env::var_os("HOME").context("HOME environment variable not set")?;
        Ok(PathBuf::from(home).join(relative))
    } else {
        Ok(PathBuf::from(path))
    }
}

/// Send a command to the daemon via Unix socket and get response
async fn send_daemon_command(command: DaemonCommand) -> Result<Response> {
    send_daemon_command_at(&socket_path()?, command).await
}

const CONTROL_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const TRIGGER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_DAEMON_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

struct DaemonExchangeLimits {
    request_timeout: Duration,
    trigger_response_timeout: Duration,
    max_response_bytes: usize,
}

async fn send_daemon_command_at(socket_path: &Path, command: DaemonCommand) -> Result<Response> {
    exchange_daemon_command(
        socket_path,
        command,
        DaemonExchangeLimits {
            request_timeout: CONTROL_COMMAND_TIMEOUT,
            trigger_response_timeout: TRIGGER_RESPONSE_TIMEOUT,
            max_response_bytes: MAX_DAEMON_RESPONSE_BYTES,
        },
    )
    .await
}

async fn exchange_daemon_command(
    socket_path: &Path,
    command: DaemonCommand,
    limits: DaemonExchangeLimits,
) -> Result<Response> {
    let triggered = matches!(command, DaemonCommand::Trigger);
    let request_deadline = Instant::now() + limits.request_timeout;
    let mut stream = timeout_at(request_deadline, async {
        let mut stream = UnixStream::connect(socket_path).await.with_context(|| format!(
            "Failed to connect to daemon socket: {}\nIs the daemon running? Start it with: systemctl --user start autogit-daemon",
            socket_path.display()
        ))?;
        let command_json = command.to_json().context("Failed to serialize command")?;
        stream.write_all(command_json.as_bytes()).await.context("Failed to send command to daemon")?;
        stream.flush().await.context("Failed to flush socket")?;
        stream.shutdown().await.context("Failed to shutdown write side of socket")?;
        Ok::<_, anyhow::Error>(stream)
    }).await.context("Timed out sending command to daemon; any accepted work may continue")??;

    let response_deadline = if triggered {
        Instant::now() + limits.trigger_response_timeout
    } else {
        request_deadline
    };
    let timeout_message = if triggered {
        "Timed out waiting for synchronisation results; daemon work may continue"
    } else {
        "Timed out waiting for daemon response"
    };
    let mut reader = BufReader::new(&mut stream).take((limits.max_response_bytes + 1) as u64);
    let mut line = Vec::new();
    timeout_at(response_deadline, reader.read_until(b'\n', &mut line))
        .await
        .context(timeout_message)?
        .context("Failed to read response from daemon")?;
    if line.len() > limits.max_response_bytes {
        bail!(
            "Daemon response exceeds {} bytes",
            limits.max_response_bytes
        );
    }
    if line.is_empty() {
        bail!("Daemon closed connection without sending response");
    }
    let line = std::str::from_utf8(&line).context("Daemon response is not valid UTF-8")?;
    Response::from_json(line).context("Failed to parse daemon response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper to create a temporary git repository
    fn create_temp_git_repo() -> Result<TempDir> {
        let temp_dir = TempDir::new()?;
        let git_dir = temp_dir.path().join(".git");
        fs::create_dir(&git_dir)?;
        Ok(temp_dir)
    }

    /// Create isolated configuration and repository directories.
    fn setup_test_env() -> Result<(TempDir, TempDir)> {
        let config_dir = TempDir::new()?;
        let repo_dir = create_temp_git_repo()?;

        Ok((config_dir, repo_dir))
    }

    #[test]
    fn test_expand_path_absolute() {
        assert_eq!(
            expand_path("/tmp").unwrap(),
            PathBuf::from("/tmp").canonicalize().unwrap()
        );
    }

    #[test]
    fn test_add_repository_creates_config() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        let result = add_repository_at(
            &config_dir.path().join("config.toml"),
            repo_path,
            None,
            None,
        );
        assert!(result.is_ok());

        // Verify config was created
        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.repositories.len(), 1);
        assert!(
            config.repositories[0]
                .path
                .to_str()
                .unwrap()
                .contains("tmp")
        );
        assert!(config.repositories[0].auto_commit);

        drop(config_dir);
    }

    #[test]
    fn test_add_repository_with_custom_message() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        let result = add_repository_at(
            &config_dir.path().join("config.toml"),
            repo_path,
            Some("Custom: {date}".to_owned()),
            None,
        );
        assert!(result.is_ok());

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(
            config.repositories[0].commit_message_template,
            "Custom: {date}"
        );

        drop(config_dir);
    }

    #[test]
    fn test_add_repository_with_interval() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        let result = add_repository_at(
            &config_dir.path().join("config.toml"),
            repo_path,
            None,
            Some(60),
        );
        assert!(result.is_ok());

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.daemon.check_interval_seconds, 60);

        drop(config_dir);
    }

    #[test]
    fn test_add_repository_not_git_repo() {
        let config_dir = TempDir::new().unwrap();

        let non_git_dir = TempDir::new().unwrap();
        let path = non_git_dir.path().to_str().unwrap();

        let result = add_repository_at(&config_dir.path().join("config.toml"), path, None, None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Not a git repository")
        );
    }

    #[test]
    fn test_add_repository_duplicate() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        // Add first time
        let result1 = add_repository_at(
            &config_dir.path().join("config.toml"),
            repo_path,
            None,
            None,
        );
        assert!(result1.is_ok());

        // Try to add again
        let result2 = add_repository_at(
            &config_dir.path().join("config.toml"),
            repo_path,
            None,
            None,
        );
        assert!(result2.is_err());
        assert!(
            result2
                .unwrap_err()
                .to_string()
                .contains("already configured")
        );

        drop(config_dir);
    }

    #[test]
    fn test_remove_repository() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        // Add repository
        add_repository_at(
            &config_dir.path().join("config.toml"),
            repo_path,
            None,
            None,
        )
        .unwrap();

        // Remove it
        let result = remove_repository_at(&config_dir.path().join("config.toml"), repo_path);
        assert!(result.is_ok());

        // Verify it's gone
        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.repositories.len(), 0);

        drop(config_dir);
    }

    #[test]
    fn test_remove_repository_not_found() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        let result = remove_repository_at(&config_dir.path().join("config.toml"), repo_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));

        drop(config_dir);
    }

    #[test]
    fn test_enable_repository() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        // Add and then disable
        add_repository_at(
            &config_dir.path().join("config.toml"),
            repo_path,
            None,
            None,
        )
        .unwrap();
        update_repository_status_at(&config_dir.path().join("config.toml"), repo_path, false)
            .unwrap();

        // Now enable
        let result =
            update_repository_status_at(&config_dir.path().join("config.toml"), repo_path, true);
        assert!(result.is_ok());

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert!(config.repositories[0].auto_commit);

        drop(config_dir);
    }

    #[test]
    fn test_disable_repository() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        // Add repository
        add_repository_at(
            &config_dir.path().join("config.toml"),
            repo_path,
            None,
            None,
        )
        .unwrap();

        // Disable it
        let result =
            update_repository_status_at(&config_dir.path().join("config.toml"), repo_path, false);
        assert!(result.is_ok());

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert!(!config.repositories[0].auto_commit);

        drop(config_dir);
    }

    #[test]
    fn test_enable_repository_not_found() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        let result =
            update_repository_status_at(&config_dir.path().join("config.toml"), repo_path, true);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));

        drop(config_dir);
    }

    #[test]
    fn test_set_interval_new_value() {
        let config_dir = TempDir::new().unwrap();

        let result = set_interval_at(&config_dir.path().join("config.toml"), Some(120));
        assert!(result.is_ok());

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.daemon.check_interval_seconds, 120);
    }

    #[test]
    fn test_set_interval_show_current() {
        let config_dir = TempDir::new().unwrap();

        // Set a value first
        set_interval_at(&config_dir.path().join("config.toml"), Some(180)).unwrap();

        // Show current (no arg) - should not error
        let result = set_interval_at(&config_dir.path().join("config.toml"), None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_repositories_empty() {
        let config_dir = TempDir::new().unwrap();

        let result = list_repositories_at(&config_dir.path().join("config.toml"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_repositories_with_repos() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        add_repository_at(
            &config_dir.path().join("config.toml"),
            repo_path,
            Some("Test message".to_owned()),
            None,
        )
        .unwrap();

        let result = list_repositories_at(&config_dir.path().join("config.toml"));
        assert!(result.is_ok());

        drop(config_dir);
    }

    #[test]
    fn test_multiple_repositories() {
        let config_dir = TempDir::new().unwrap();

        let repo1 = create_temp_git_repo().unwrap();
        let repo2 = create_temp_git_repo().unwrap();

        add_repository_at(
            &config_dir.path().join("config.toml"),
            repo1.path().to_str().unwrap(),
            None,
            None,
        )
        .unwrap();
        add_repository_at(
            &config_dir.path().join("config.toml"),
            repo2.path().to_str().unwrap(),
            Some("Custom".to_owned()),
            None,
        )
        .unwrap();

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.repositories.len(), 2);
        assert_eq!(config.repositories[1].commit_message_template, "Custom");
    }

    #[test]
    fn test_remove_one_of_multiple_repositories() {
        let config_dir = TempDir::new().unwrap();

        let repo1 = create_temp_git_repo().unwrap();
        let repo2 = create_temp_git_repo().unwrap();

        add_repository_at(
            &config_dir.path().join("config.toml"),
            repo1.path().to_str().unwrap(),
            None,
            None,
        )
        .unwrap();
        add_repository_at(
            &config_dir.path().join("config.toml"),
            repo2.path().to_str().unwrap(),
            None,
            None,
        )
        .unwrap();

        remove_repository_at(
            &config_dir.path().join("config.toml"),
            repo1.path().to_str().unwrap(),
        )
        .unwrap();

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.repositories.len(), 1);
        assert_eq!(
            config.repositories[0].path,
            repo2.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn test_disable_then_enable_repository() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        add_repository_at(
            &config_dir.path().join("config.toml"),
            repo_path,
            None,
            None,
        )
        .unwrap();

        // Disable
        update_repository_status_at(&config_dir.path().join("config.toml"), repo_path, false)
            .unwrap();
        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert!(!config.repositories[0].auto_commit);

        // Enable
        update_repository_status_at(&config_dir.path().join("config.toml"), repo_path, true)
            .unwrap();
        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert!(config.repositories[0].auto_commit);

        drop(config_dir);
    }

    #[test]
    fn test_interval_persists_across_operations() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        // Set interval
        set_interval_at(&config_dir.path().join("config.toml"), Some(240)).unwrap();

        // Add repository (shouldn't change interval)
        add_repository_at(
            &config_dir.path().join("config.toml"),
            repo_path,
            None,
            None,
        )
        .unwrap();

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.daemon.check_interval_seconds, 240);

        drop(config_dir);
    }

    #[test]
    fn test_add_repository_updates_interval() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap();

        // Add repository with interval
        add_repository_at(
            &config_dir.path().join("config.toml"),
            repo_path,
            None,
            Some(90),
        )
        .unwrap();

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.daemon.check_interval_seconds, 90);

        drop(config_dir);
    }

    #[test]
    fn test_config_survives_operations() {
        let (config_dir, repo_dir1) = setup_test_env().unwrap();
        let repo_dir2 = create_temp_git_repo().unwrap();

        let path1 = repo_dir1.path().to_str().unwrap();
        let path2 = repo_dir2.path().to_str().unwrap();

        // Add first repo
        add_repository_at(
            &config_dir.path().join("config.toml"),
            path1,
            Some("Msg1".to_owned()),
            Some(60),
        )
        .unwrap();

        // Add second repo
        add_repository_at(
            &config_dir.path().join("config.toml"),
            path2,
            Some("Msg2".to_owned()),
            None,
        )
        .unwrap();

        // Disable first
        update_repository_status_at(&config_dir.path().join("config.toml"), path1, false).unwrap();

        // Verify final state
        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.repositories.len(), 2);
        assert!(!config.repositories[0].auto_commit);
        assert!(config.repositories[1].auto_commit);
        assert_eq!(config.repositories[0].commit_message_template, "Msg1");
        assert_eq!(config.repositories[1].commit_message_template, "Msg2");

        drop(config_dir);
    }

    #[tokio::test]
    async fn test_is_daemon_running_no_socket() {
        let config_dir = TempDir::new().unwrap();

        // Should return false when socket doesn't exist
        let response =
            send_daemon_command_at(&config_dir.path().join("daemon.sock"), DaemonCommand::Ping)
                .await;
        assert!(response.is_err());

        drop(config_dir);
    }

    #[tokio::test]
    async fn test_send_daemon_command_no_daemon() {
        let config_dir = TempDir::new().unwrap();

        // Should fail when daemon is not running
        let result =
            send_daemon_command_at(&config_dir.path().join("daemon.sock"), DaemonCommand::Ping)
                .await;
        assert!(result.is_err());

        drop(config_dir);
    }

    #[test]
    fn test_list_repositories_formatting() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let path = repo_dir.path().to_str().unwrap();

        add_repository_at(
            &config_dir.path().join("config.toml"),
            path,
            Some("Test message".to_owned()),
            None,
        )
        .unwrap();

        // List should succeed (even though we can't easily test the output format)
        let result = list_repositories_at(&config_dir.path().join("config.toml"));
        assert!(result.is_ok());

        drop(config_dir);
        drop(repo_dir);
    }

    #[test]
    fn test_remove_repository_cleans_up_empty_config() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let path = repo_dir.path().to_str().unwrap();

        add_repository_at(&config_dir.path().join("config.toml"), path, None, None).unwrap();

        // Remove it
        remove_repository_at(&config_dir.path().join("config.toml"), path).unwrap();

        // Config should have no repositories
        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.repositories.len(), 0);

        drop(config_dir);
        drop(repo_dir);
    }

    #[test]
    fn test_enable_already_enabled_repository() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let path = repo_dir.path().to_str().unwrap();

        add_repository_at(&config_dir.path().join("config.toml"), path, None, None).unwrap();

        // Enable it (should already be enabled)
        let result =
            update_repository_status_at(&config_dir.path().join("config.toml"), path, true);
        assert!(result.is_ok());

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert!(config.repositories[0].auto_commit);

        drop(config_dir);
        drop(repo_dir);
    }

    #[test]
    fn test_disable_already_disabled_repository() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let path = repo_dir.path().to_str().unwrap();

        add_repository_at(&config_dir.path().join("config.toml"), path, None, None).unwrap();

        // Disable it
        update_repository_status_at(&config_dir.path().join("config.toml"), path, false).unwrap();

        // Disable it again (should be idempotent)
        let result =
            update_repository_status_at(&config_dir.path().join("config.toml"), path, false);
        assert!(result.is_ok());

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert!(!config.repositories[0].auto_commit);

        drop(config_dir);
        drop(repo_dir);
    }

    #[test]
    fn test_disable_repository_not_found() {
        let config_dir = TempDir::new().unwrap();

        let result = update_repository_status_at(
            &config_dir.path().join("config.toml"),
            "/nonexistent/path",
            false,
        );
        assert!(result.is_err());
        let err = result.err().unwrap().to_string();
        assert!(err.contains("Repository not found") || err.contains("Failed to resolve path"));

        drop(config_dir);
    }

    #[test]
    fn test_set_interval_zero() {
        let config_dir = TempDir::new().unwrap();

        // Invalid changes must not overwrite the persisted configuration.
        set_interval_at(&config_dir.path().join("config.toml"), Some(60)).unwrap();
        let result = set_interval_at(&config_dir.path().join("config.toml"), Some(0));
        assert!(result.is_err());

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.daemon.check_interval_seconds, 60);

        drop(config_dir);
    }

    #[test]
    fn test_set_interval_large_value() {
        let config_dir = TempDir::new().unwrap();

        // Should allow large intervals
        let result = set_interval_at(&config_dir.path().join("config.toml"), Some(86400)); // 24 hours
        assert!(result.is_ok());

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.daemon.check_interval_seconds, 86400);

        drop(config_dir);
    }

    #[test]
    fn test_add_repository_preserves_other_repos() {
        let config_dir = TempDir::new().unwrap();

        // Create two test git repos
        let repo1 = create_temp_git_repo().unwrap();
        let repo2 = create_temp_git_repo().unwrap();

        let path1 = repo1.path().to_str().unwrap();
        let path2 = repo2.path().to_str().unwrap();

        // Add first repo
        add_repository_at(
            &config_dir.path().join("config.toml"),
            path1,
            Some("Message 1".to_owned()),
            None,
        )
        .unwrap();

        // Add second repo
        add_repository_at(
            &config_dir.path().join("config.toml"),
            path2,
            Some("Message 2".to_owned()),
            None,
        )
        .unwrap();

        // Verify both exist
        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.repositories.len(), 2);
        assert_eq!(config.repositories[0].commit_message_template, "Message 1");
        assert_eq!(config.repositories[1].commit_message_template, "Message 2");

        drop(config_dir);
        drop(repo1);
        drop(repo2);
    }

    #[test]
    fn test_remove_repository_preserves_others() {
        let config_dir = TempDir::new().unwrap();

        // Create three test git repos
        let repo1 = create_temp_git_repo().unwrap();
        let repo2 = create_temp_git_repo().unwrap();
        let repo3 = create_temp_git_repo().unwrap();

        let path1 = repo1.path().to_str().unwrap();
        let path2 = repo2.path().to_str().unwrap();
        let path3 = repo3.path().to_str().unwrap();

        // Add all three
        add_repository_at(
            &config_dir.path().join("config.toml"),
            path1,
            Some("Msg1".to_owned()),
            None,
        )
        .unwrap();
        add_repository_at(
            &config_dir.path().join("config.toml"),
            path2,
            Some("Msg2".to_owned()),
            None,
        )
        .unwrap();
        add_repository_at(
            &config_dir.path().join("config.toml"),
            path3,
            Some("Msg3".to_owned()),
            None,
        )
        .unwrap();

        // Remove middle one
        remove_repository_at(&config_dir.path().join("config.toml"), path2).unwrap();

        // Verify only first and third remain
        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(config.repositories.len(), 2);
        assert_eq!(config.repositories[0].commit_message_template, "Msg1");
        assert_eq!(config.repositories[1].commit_message_template, "Msg3");

        drop(config_dir);
        drop(repo1);
        drop(repo2);
        drop(repo3);
    }

    #[test]
    fn test_add_repository_default_message_template() {
        let (config_dir, repo_dir) = setup_test_env().unwrap();
        let path = repo_dir.path().to_str().unwrap();

        // Add without specifying message template
        add_repository_at(&config_dir.path().join("config.toml"), path, None, None).unwrap();

        let config = Config::load_or_create(config_dir.path().join("config.toml")).unwrap();
        assert_eq!(
            config.repositories[0].commit_message_template,
            "Auto-commit: {timestamp}"
        );

        drop(config_dir);
        drop(repo_dir);
    }

    #[test]
    fn test_update_repository_status_not_found() {
        let config_dir = TempDir::new().unwrap();

        // Try to update non-existent repository
        let result = update_repository_status_at(
            &config_dir.path().join("config.toml"),
            "/nonexistent",
            true,
        );
        assert!(result.is_err());
        let err = result.err().unwrap().to_string();
        assert!(err.contains("Repository not found") || err.contains("Failed to resolve path"));

        drop(config_dir);
    }

    #[test]
    fn test_edit_config_creates_config() {
        let config_dir = TempDir::new().unwrap();

        // Should create config if it doesn't exist
        let result = edit_config_at(&config_dir.path().join("config.toml"), "true");
        assert!(result.is_ok());

        // Verify config was created
        let config_path = config_dir.path().join("config.toml");
        assert!(config_path.exists());

        drop(config_dir);
    }

    #[tokio::test]
    async fn test_show_status_no_daemon() {
        let config_dir = TempDir::new().unwrap();

        // Should handle daemon not running gracefully
        let result = show_status_at(
            &config_dir.path().join("config.toml"),
            &config_dir.path().join("daemon.sock"),
        )
        .await;
        // It's okay if this fails since daemon is not running
        // Just testing that it doesn't panic
        let _ = result;

        drop(config_dir);
    }

    #[tokio::test]
    async fn test_trigger_now_no_daemon() {
        let config_dir = TempDir::new().unwrap();

        // Should fail when daemon is not running
        let result = send_daemon_command_at(
            &config_dir.path().join("daemon.sock"),
            DaemonCommand::Trigger,
        )
        .await;
        assert!(result.is_err());

        drop(config_dir);
    }

    #[tokio::test]
    async fn test_suspend_daemon_no_daemon() {
        let config_dir = TempDir::new().unwrap();

        // Should fail when daemon is not running
        let result = send_daemon_command_at(
            &config_dir.path().join("daemon.sock"),
            DaemonCommand::Suspend,
        )
        .await;
        assert!(result.is_err());

        drop(config_dir);
    }

    #[tokio::test]
    async fn test_resume_daemon_no_daemon() {
        let config_dir = TempDir::new().unwrap();

        // Should fail when daemon is not running
        let result = send_daemon_command_at(
            &config_dir.path().join("daemon.sock"),
            DaemonCommand::Resume,
        )
        .await;
        assert!(result.is_err());

        drop(config_dir);
    }
    #[test]
    fn missing_repository_can_be_disabled_and_removed() {
        let config_dir = TempDir::new().unwrap();
        let config_path = config_dir.path().join("config.toml");
        let repo = create_temp_git_repo().unwrap();
        let repo_path = repo.path().to_owned();
        let path = repo_path.to_str().unwrap();
        add_repository_at(&config_path, path, None, None).unwrap();
        drop(repo);
        update_repository_status_at(&config_path, path, false).unwrap();
        assert!(!Config::load(&config_path).unwrap().repositories[0].auto_commit);
        remove_repository_at(&config_path, path).unwrap();
        assert!(Config::load(&config_path).unwrap().repositories.is_empty());
    }

    #[test]
    fn missing_repository_identity_resolves_symlink_ancestor() {
        let directory = TempDir::new().unwrap();
        let target = directory.path().join("target");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, directory.path().join("alias")).unwrap();
        let path = directory.path().join("alias/missing/../repository");
        assert_eq!(
            repository_identity(path.to_str().unwrap()).unwrap(),
            target.join("repository")
        );
    }

    #[test]
    fn failed_trigger_with_details_returns_error() {
        let response = Response {
            status: ResponseStatus::Error,
            message: "Push failed".to_owned(),
            data: Some(ResponseData::Trigger {
                repos_checked: 1,
                repos_committed: 1,
                details: vec![autogit_shared::RepoDetail {
                    path: PathBuf::from("/repository"),
                    committed: true,
                    files_changed: Some(1),
                    error: Some("Push failed".to_owned()),
                }],
            }),
        };
        assert!(display_trigger_result(response).is_err());
    }

    #[test]
    fn successful_response_with_repository_error_still_fails() {
        let response = Response::ok_with_data(
            "Checked",
            ResponseData::Trigger {
                repos_checked: 1,
                repos_committed: 0,
                details: vec![autogit_shared::RepoDetail {
                    path: PathBuf::from("/repository"),
                    committed: false,
                    files_changed: None,
                    error: Some("Pull failed".to_owned()),
                }],
            },
        );
        assert!(display_trigger_result(response).is_err());
    }

    #[test]
    fn status_uses_applied_daemon_configuration() {
        let mut config = Config::default();
        config.daemon.check_interval_seconds = 123;
        let response = Response::ok_with_data(
            "Status",
            ResponseData::Status {
                uptime_seconds: 12,
                check_interval_seconds: 123,
                repositories_count: 0,
                config,
                suspended: true,
                syncing: false,
                details: vec![],
            },
        );
        assert!(display_daemon_status(response).is_ok());
        assert!(display_daemon_status(Response::ok("Missing status")).is_err());
    }
    #[tokio::test]
    async fn running_daemon_status_does_not_load_unapplied_disk_config() {
        let directory = TempDir::new().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "invalid TOML").unwrap();
        let socket_path = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            stream.read_line(&mut request).await.unwrap();
            assert!(matches!(
                DaemonCommand::from_json(&request).unwrap(),
                DaemonCommand::Status
            ));
            let response = Response::ok_with_data(
                "Status",
                ResponseData::Status {
                    uptime_seconds: 20,
                    check_interval_seconds: 300,
                    repositories_count: 0,
                    config: Config::default(),
                    suspended: true,
                    syncing: false,
                    details: vec![],
                },
            );
            stream
                .get_mut()
                .write_all(response.to_json().unwrap().as_bytes())
                .await
                .unwrap();
        });
        show_status_at(&config_path, &socket_path).await.unwrap();
        server.await.unwrap();
    }
    #[test]
    fn editor_command_supports_quoted_arguments_without_a_shell() {
        let directory = TempDir::new().unwrap();
        let config_path = directory.path().join("config.toml");
        let script = directory.path().join("editor with spaces.py");
        fs::write(&script, "import sys\nfrom pathlib import Path\nassert sys.argv[1:3] == ['--label', 'argument with spaces']\nPath(sys.argv[3]).write_text('[daemon]\\ncheck_interval_seconds = 42\\n')\n").unwrap();
        let editor = shell_words::join([
            "python3",
            script.to_str().unwrap(),
            "--label",
            "argument with spaces",
        ]);
        edit_config_at(&config_path, &editor).unwrap();
        assert_eq!(
            Config::load(&config_path)
                .unwrap()
                .daemon
                .check_interval_seconds,
            42
        );
    }

    #[test]
    fn failed_and_malformed_editors_leave_configuration_unchanged() {
        let directory = TempDir::new().unwrap();
        let config_path = directory.path().join("config.toml");
        Config::default().save(&config_path).unwrap();
        let original = fs::read(&config_path).unwrap();
        let failing_editor = shell_words::join(["python3", "-c", "import sys; sys.exit(7)"]);
        for editor in [failing_editor.as_str(), "", "unterminated '"] {
            assert!(edit_config_at(&config_path, editor).is_err());
            assert_eq!(fs::read(&config_path).unwrap(), original);
        }
    }

    #[tokio::test]
    async fn daemon_control_response_has_a_deadline() {
        let directory = TempDir::new().unwrap();
        let socket_path = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            wait.await.unwrap();
        });
        let error = exchange_daemon_command(
            &socket_path,
            DaemonCommand::Status,
            DaemonExchangeLimits {
                request_timeout: Duration::from_millis(50),
                trigger_response_timeout: Duration::from_secs(1),
                max_response_bytes: 1024,
            },
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Timed out waiting for daemon response")
        );
        release.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn trigger_timeout_explains_that_work_may_continue() {
        let directory = TempDir::new().unwrap();
        let socket_path = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            assert!(matches!(
                DaemonCommand::from_json(&request).unwrap(),
                DaemonCommand::Trigger
            ));
            wait.await.unwrap();
        });
        let error = exchange_daemon_command(
            &socket_path,
            DaemonCommand::Trigger,
            DaemonExchangeLimits {
                request_timeout: Duration::from_secs(1),
                trigger_response_timeout: Duration::from_millis(20),
                max_response_bytes: 1024,
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("daemon work may continue"));
        release.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_daemon_response_is_rejected_before_peer_closes() {
        let directory = TempDir::new().unwrap();
        let socket_path = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            reader.get_mut().write_all(&[b'x'; 65]).await.unwrap();
            wait.await.unwrap();
        });
        let error = exchange_daemon_command(
            &socket_path,
            DaemonCommand::Ping,
            DaemonExchangeLimits {
                request_timeout: Duration::from_secs(1),
                trigger_response_timeout: Duration::from_secs(1),
                max_response_bytes: 64,
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("exceeds 64 bytes"));
        release.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn trigger_response_can_outlast_control_deadline() {
        let directory = TempDir::new().unwrap();
        let socket_path = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            reader
                .get_mut()
                .write_all(Response::ok("Finished").to_json().unwrap().as_bytes())
                .await
                .unwrap();
        });
        let response = exchange_daemon_command(
            &socket_path,
            DaemonCommand::Trigger,
            DaemonExchangeLimits {
                request_timeout: Duration::from_millis(50),
                trigger_response_timeout: Duration::from_secs(1),
                max_response_bytes: 1024,
            },
        )
        .await
        .unwrap();
        assert_eq!(response.message, "Finished");
        server.await.unwrap();
    }
}
