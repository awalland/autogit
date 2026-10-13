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

        /// Global check interval in seconds (optional, keeps the existing interval)
        #[arg(short, long, value_parser = clap::value_parser!(u64).range(1..))]
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
        #[arg(value_parser = clap::value_parser!(u64).range(1..))]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_intervals_are_rejected_before_command_execution() {
        assert!(Cli::try_parse_from(["autogit", "interval", "0"]).is_err());
        assert!(Cli::try_parse_from(["autogit", "add", "/repository", "--interval", "0"]).is_err());
        assert!(Cli::try_parse_from(["autogit", "interval", "1"]).is_ok());
    }
}
