mod commands;
mod cli;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Add { path, message, interval } => {
            commands::add_repository(&path, message, interval)?;
        }
        Commands::Remove { path } => {
            commands::remove_repository(&path)?;
        }
        Commands::List => {
            commands::list_repositories()?;
        }
        Commands::Enable { path } => {
            commands::enable_repository(&path)?;
        }
        Commands::Disable { path } => {
            commands::disable_repository(&path)?;
        }
        Commands::Interval { seconds } => {
            commands::set_interval(seconds)?;
        }
        Commands::Status => {
            commands::show_status().await?;
        }
        Commands::Edit => {
            commands::edit_config()?;
        }
        Commands::Now => {
            commands::trigger_now().await?;
        }
        Commands::Suspend => {
            commands::suspend_daemon().await?;
        }
        Commands::Resume => {
            commands::resume_daemon().await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::env;
    use tempfile::TempDir;
    use serial_test::serial;

    #[test]
    fn test_cli_parse_add_basic() {
        let cli = Cli::parse_from(["autogit", "add", "/tmp/repo"]);
        match cli.command {
            Commands::Add { path, message, interval } => {
                assert_eq!(path, "/tmp/repo");
                assert_eq!(message, None);
                assert_eq!(interval, None);
            }
            _ => panic!("Expected Add command"),
        }
    }

    #[test]
    fn test_cli_parse_add_with_message() {
        let cli = Cli::parse_from([
            "autogit",
            "add",
            "/tmp/repo",
            "--message",
            "Custom commit: {timestamp}",
        ]);
        match cli.command {
            Commands::Add { path, message, interval } => {
                assert_eq!(path, "/tmp/repo");
                assert_eq!(message, Some("Custom commit: {timestamp}".to_owned()));
                assert_eq!(interval, None);
            }
            _ => panic!("Expected Add command"),
        }
    }

    #[test]
    fn test_cli_parse_add_with_interval() {
        let cli = Cli::parse_from(["autogit", "add", "/tmp/repo", "--interval", "600"]);
        match cli.command {
            Commands::Add { path, message, interval } => {
                assert_eq!(path, "/tmp/repo");
                assert_eq!(message, None);
                assert_eq!(interval, Some(600));
            }
            _ => panic!("Expected Add command"),
        }
    }

    #[test]
    fn test_cli_parse_add_with_all_options() {
        let cli = Cli::parse_from([
            "autogit",
            "add",
            "/tmp/repo",
            "-m",
            "Test message",
            "-i",
            "900",
        ]);
        match cli.command {
            Commands::Add { path, message, interval } => {
                assert_eq!(path, "/tmp/repo");
                assert_eq!(message, Some("Test message".to_owned()));
                assert_eq!(interval, Some(900));
            }
            _ => panic!("Expected Add command"),
        }
    }

    #[test]
    fn test_cli_parse_remove() {
        let cli = Cli::parse_from(["autogit", "remove", "/tmp/repo"]);
        match cli.command {
            Commands::Remove { path } => {
                assert_eq!(path, "/tmp/repo");
            }
            _ => panic!("Expected Remove command"),
        }
    }

    #[test]
    fn test_cli_parse_list() {
        let cli = Cli::parse_from(["autogit", "list"]);
        matches!(cli.command, Commands::List);
    }

    #[test]
    fn test_cli_parse_enable() {
        let cli = Cli::parse_from(["autogit", "enable", "/tmp/repo"]);
        match cli.command {
            Commands::Enable { path } => {
                assert_eq!(path, "/tmp/repo");
            }
            _ => panic!("Expected Enable command"),
        }
    }

    #[test]
    fn test_cli_parse_disable() {
        let cli = Cli::parse_from(["autogit", "disable", "/tmp/repo"]);
        match cli.command {
            Commands::Disable { path } => {
                assert_eq!(path, "/tmp/repo");
            }
            _ => panic!("Expected Disable command"),
        }
    }

    #[test]
    fn test_cli_parse_interval_show() {
        let cli = Cli::parse_from(["autogit", "interval"]);
        match cli.command {
            Commands::Interval { seconds } => {
                assert_eq!(seconds, None);
            }
            _ => panic!("Expected Interval command"),
        }
    }

    #[test]
    fn test_cli_parse_interval_set() {
        let cli = Cli::parse_from(["autogit", "interval", "300"]);
        match cli.command {
            Commands::Interval { seconds } => {
                assert_eq!(seconds, Some(300));
            }
            _ => panic!("Expected Interval command"),
        }
    }

    #[test]
    fn test_cli_parse_status() {
        let cli = Cli::parse_from(["autogit", "status"]);
        matches!(cli.command, Commands::Status);
    }

    #[test]
    fn test_cli_parse_edit() {
        let cli = Cli::parse_from(["autogit", "edit"]);
        matches!(cli.command, Commands::Edit);
    }

    #[test]
    fn test_cli_parse_now() {
        let cli = Cli::parse_from(["autogit", "now"]);
        matches!(cli.command, Commands::Now);
    }

    #[test]
    fn test_cli_parse_suspend() {
        let cli = Cli::parse_from(["autogit", "suspend"]);
        matches!(cli.command, Commands::Suspend);
    }

    #[test]
    fn test_cli_parse_resume() {
        let cli = Cli::parse_from(["autogit", "resume"]);
        matches!(cli.command, Commands::Resume);
    }

    // Integration tests that execute main logic
    #[test]
    #[serial]
    fn test_main_add_command() {
        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Create a temp git repo
        let repo_dir = temp_dir.path().join("test_repo");
        std::fs::create_dir(&repo_dir).unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();

        // Test add command through CLI parsing
        let result = commands::add_repository(&repo_dir.to_string_lossy(), None, None);
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_main_list_command() {
        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Should work even with no config
        let result = commands::list_repositories();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_main_interval_command() {
        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Test showing interval
        let result = commands::set_interval(None);
        assert!(result.is_ok());

        // Test setting interval
        let result = commands::set_interval(Some(600));
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_main_enable_disable_commands() {
        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Create a test repo
        let repo_dir = temp_dir.path().join("test_repo");
        std::fs::create_dir(&repo_dir).unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();

        // Add it first
        commands::add_repository(&repo_dir.to_string_lossy(), None, None).unwrap();

        // Test disable
        let result = commands::disable_repository(&repo_dir.to_string_lossy());
        assert!(result.is_ok());

        // Test enable
        let result = commands::enable_repository(&repo_dir.to_string_lossy());
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_main_remove_command() {
        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", temp_dir.path());

        // Create a test repo
        let repo_dir = temp_dir.path().join("test_repo");
        std::fs::create_dir(&repo_dir).unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();

        // Add it first
        commands::add_repository(&repo_dir.to_string_lossy(), None, None).unwrap();

        // Test remove
        let result = commands::remove_repository(&repo_dir.to_string_lossy());
        assert!(result.is_ok());
    }
}
