use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub daemon: DaemonConfig,

    #[serde(default)]
    pub repositories: Vec<Repository>,
}

/// Daemon-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// How often to check for changes (in seconds)
    #[serde(default = "default_check_interval")]
    pub check_interval_seconds: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            check_interval_seconds: default_check_interval(),
        }
    }
}

fn default_check_interval() -> u64 {
    300 // 5 minutes
}

/// Repository configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    /// Path to the git repository
    pub path: PathBuf,

    /// Whether auto-commit is enabled for this repo
    #[serde(default = "default_true")]
    pub auto_commit: bool,

    /// Template for commit messages
    /// Available placeholders: {timestamp}, {date}, {time}
    #[serde(default = "default_commit_message")]
    pub commit_message_template: String,
}

fn default_true() -> bool {
    true
}

fn default_commit_message() -> String {
    "Auto-commit: {timestamp}".to_string()
}

impl Config {
    /// Load configuration from a TOML file
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read config file: {}", path.as_ref().display()))?;

        let config: Config = toml::from_str(&content)
            .with_context(|| "Failed to parse config file")?;

        Ok(config)
    }

    /// Save configuration to a TOML file
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let content = toml::to_string_pretty(self)
            .with_context(|| "Failed to serialize config")?;

        // Ensure parent directory exists
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
        }

        std::fs::write(path.as_ref(), content)
            .with_context(|| format!("Failed to write config file: {}", path.as_ref().display()))?;

        Ok(())
    }

    /// Get the default config file path (~/.config/autogit/config.toml)
    pub fn default_config_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .context("Could not determine config directory")?;

        Ok(config_dir.join("autogit").join("config.toml"))
    }

    /// Load config from default location, or create a default one if it doesn't exist
    pub fn load_or_create_default() -> Result<Self> {
        let path = Self::default_config_path()?;

        if path.exists() {
            Self::load(&path)
        } else {
            let config = Self::default();
            config.save(&path)?;
            Ok(config)
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon: DaemonConfig::default(),
            repositories: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_serialization() {
        let config = Config {
            daemon: DaemonConfig {
                check_interval_seconds: 60,
            },
            repositories: vec![
                Repository {
                    path: PathBuf::from("/home/user/notes"),
                    auto_commit: true,
                    commit_message_template: "Auto-commit: {timestamp}".to_string(),
                },
            ],
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(deserialized.daemon.check_interval_seconds, 60);
        assert_eq!(deserialized.repositories.len(), 1);
    }
}
