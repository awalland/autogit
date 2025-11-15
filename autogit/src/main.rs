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
    }

    Ok(())
}
