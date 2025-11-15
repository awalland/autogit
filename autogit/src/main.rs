mod commands;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "autogit")]
#[command(about = "Configuration tool for autogit-daemon", long_about = None)]
#[command(version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Add a repository to auto-commit
    Add {
        /// Path to the git repository
        path: String,

        /// Commit message template (default: "Auto-commit: {timestamp}")
        #[arg(short, long)]
        message: Option<String>,

        /// Check interval in seconds (optional, uses daemon default if not set)
        #[arg(short, long)]
        interval: Option<u64>,
    },

    /// Remove a repository from auto-commit
    Remove {
        /// Path to the repository to remove
        path: String,
    },

    /// List all configured repositories
    List,

    /// Enable auto-commit for a repository
    Enable {
        /// Path to the repository
        path: String,
    },

    /// Disable auto-commit for a repository
    Disable {
        /// Path to the repository
        path: String,
    },

    /// Set or show the global check interval
    Interval {
        /// Interval in seconds (if not provided, shows current interval)
        seconds: Option<u64>,
    },

    /// Show current configuration
    Status,

    /// Edit configuration file in $EDITOR
    Edit,
}

fn main() -> Result<()> {
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
            commands::show_status()?;
        }
        Commands::Edit => {
            commands::edit_config()?;
        }
    }

    Ok(())
}
