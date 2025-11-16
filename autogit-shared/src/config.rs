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

    /// Whether to show system tray icon
    #[serde(default = "default_enable_tray")]
    pub enable_tray: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            check_interval_seconds: default_check_interval(),
            enable_tray: default_enable_tray(),
        }
    }
}

fn default_check_interval() -> u64 {
    300 // 5 minutes
}

fn default_enable_tray() -> bool {
    true
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
    "Auto-commit: {timestamp}".to_owned()
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
                enable_tray: true,
            },
            repositories: vec![
                Repository {
                    path: PathBuf::from("/home/user/notes"),
                    auto_commit: true,
                    commit_message_template: "Auto-commit: {timestamp}".to_owned(),
                },
            ],
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(deserialized.daemon.check_interval_seconds, 60);
        assert_eq!(deserialized.repositories.len(), 1);
    }

    #[test]
    fn test_config_defaults() {
        let config = Config::default();

        // Default daemon config
        assert_eq!(config.daemon.check_interval_seconds, 300); // 5 minutes

        // No repositories by default
        assert_eq!(config.repositories.len(), 0);
    }

    #[test]
    fn test_daemon_config_defaults() {
        let daemon_config = DaemonConfig::default();
        assert_eq!(daemon_config.check_interval_seconds, 300);
    }

    #[test]
    fn test_repository_with_defaults() {
        // Test that serde defaults work when fields are missing
        let toml_str = r#"
            path = "/home/user/repo"
        "#;

        let repo: Repository = toml::from_str(toml_str).unwrap();

        assert_eq!(repo.path, PathBuf::from("/home/user/repo"));
        assert_eq!(repo.auto_commit, true); // default_true
        assert_eq!(repo.commit_message_template, "Auto-commit: {timestamp}"); // default_commit_message
    }

    #[test]
    fn test_config_with_partial_daemon_section() {
        // Test that daemon defaults work when section is missing
        let toml_str = r#"
            [[repositories]]
            path = "/home/user/notes"
        "#;

        let config: Config = toml::from_str(toml_str).unwrap();

        assert_eq!(config.daemon.check_interval_seconds, 300); // Uses default
        assert_eq!(config.repositories.len(), 1);
    }

    #[test]
    fn test_config_with_multiple_repositories() {
        let config = Config {
            daemon: DaemonConfig {
                check_interval_seconds: 120,
                enable_tray: true,
            },
            repositories: vec![
                Repository {
                    path: PathBuf::from("/home/user/notes"),
                    auto_commit: true,
                    commit_message_template: "Notes: {date}".to_owned(),
                },
                Repository {
                    path: PathBuf::from("/home/user/journal"),
                    auto_commit: false,
                    commit_message_template: "Journal: {time}".to_owned(),
                },
                Repository {
                    path: PathBuf::from("/home/user/code"),
                    auto_commit: true,
                    commit_message_template: "Code changes".to_owned(),
                },
            ],
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(deserialized.repositories.len(), 3);
        assert_eq!(deserialized.repositories[0].auto_commit, true);
        assert_eq!(deserialized.repositories[1].auto_commit, false);
        assert_eq!(deserialized.repositories[2].commit_message_template, "Code changes");
    }

    #[test]
    fn test_config_empty_repositories() {
        let config = Config {
            daemon: DaemonConfig {
                check_interval_seconds: 60,
                enable_tray: true,
            },
            repositories: vec![],
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(deserialized.repositories.len(), 0);
    }

    #[test]
    fn test_repository_disabled_auto_commit() {
        let toml_str = r#"
            path = "/home/user/repo"
            auto_commit = false
            commit_message_template = "Custom message"
        "#;

        let repo: Repository = toml::from_str(toml_str).unwrap();

        assert_eq!(repo.auto_commit, false);
        assert_eq!(repo.commit_message_template, "Custom message");
    }

    #[test]
    fn test_config_various_intervals() {
        for interval in [1, 60, 300, 3600, 86400] {
            let config = Config {
                daemon: DaemonConfig {
                    check_interval_seconds: interval,
                    enable_tray: true,
                },
                repositories: vec![],
            };

            let toml_str = toml::to_string_pretty(&config).unwrap();
            let deserialized: Config = toml::from_str(&toml_str).unwrap();

            assert_eq!(deserialized.daemon.check_interval_seconds, interval);
        }
    }

    #[test]
    fn test_repository_custom_message_templates() {
        let templates = vec![
            "Auto-commit: {timestamp}",
            "Changes at {date} {time}",
            "Update {date}",
            "Checkpoint",
            "Work in progress: {timestamp}",
        ];

        for template in templates {
            let repo = Repository {
                path: PathBuf::from("/test"),
                auto_commit: true,
                commit_message_template: template.to_owned(),
            };

            let toml_str = toml::to_string(&repo).unwrap();
            let deserialized: Repository = toml::from_str(&toml_str).unwrap();

            assert_eq!(deserialized.commit_message_template, template);
        }
    }
}
