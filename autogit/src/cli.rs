use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "autogit")]
#[command(about = "Configuration tool for autogit-daemon", long_about = None)]
#[command(version = env!("CARGO_PKG_VERSION"))]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
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

    /// Trigger an immediate check and commit cycle
    Now,

    /// Suspend the daemon (stop automatic syncing)
    Suspend,

    /// Resume the daemon (restart automatic syncing)
    Resume,
}
