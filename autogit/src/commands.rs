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
