use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Main configuration structure
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

        ensure!(!content.trim().is_empty(), "Configuration file is empty");
        let config: Config =
            toml::from_str(&content).with_context(|| "Failed to parse config file")?;

        config.validate()?;
        Ok(config)
    }

    /// Save configuration to a TOML file
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        self.validate()?;
        let path = path.as_ref();
        let _lock = Self::lock(path)?;
        self.save_atomic(path)
    }

    pub fn validate(&self) -> Result<()> {
        let seconds = self.daemon.check_interval_seconds;
        ensure!(seconds > 0, "Check interval must be at least 1 second");
        ensure!(
            Instant::now()
                .checked_add(Duration::from_secs(seconds))
                .is_some(),
            "Check interval exceeds the platform timer range"
        );
        Ok(())
    }

    /// Apply a complete read-modify-write transaction under a stable sidecar lock.
    pub fn update<T>(
        path: impl AsRef<Path>,
        update: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let path = path.as_ref();
        let _lock = Self::lock(path)?;
        let mut config = if path.exists() {
            Self::load(path)?
        } else {
            Self::default()
        };
        let result = update(&mut config)?;
        config.validate()?;
        config.save_atomic(path)?;
        Ok(result)
    }

    fn lock(path: &Path) -> Result<File> {
        let parent = config_parent(path);
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
        let mut name = path
            .file_name()
            .context("Config path has no filename")?
            .to_os_string();
        name.push(".lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(parent.join(name))
            .context("Failed to open config lock")?;
        lock.lock().context("Failed to lock configuration")?;
        Ok(lock)
    }

    /// Edit a private copy, then validate and atomically publish it under the transaction lock.
    pub fn edit_file(path: impl AsRef<Path>, edit: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
        let path = path.as_ref();
        let _lock = Self::lock(path)?;
        let content = if path.exists() {
            std::fs::read_to_string(path).context("Failed to read configuration for editing")?
        } else {
            toml::to_string_pretty(&Self::default())?
        };
        let temporary = Self::temporary_config(path, &content)?;
        edit(temporary.path())?;
        Self::load(temporary.path()).context("Edited configuration is invalid")?;
        Self::persist_config(temporary, path)
    }

    fn save_atomic(&self, path: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self).context("Failed to serialize config")?;
        Self::persist_config(Self::temporary_config(path, &content)?, path)
    }

    fn temporary_config(path: &Path, content: &str) -> Result<tempfile::NamedTempFile> {
        let mut temporary = tempfile::Builder::new()
            .prefix(".config-")
            .suffix(".toml")
            .tempfile_in(config_parent(path))
            .context("Failed to create temporary configuration")?;
        if let Ok(metadata) = std::fs::metadata(path) {
            temporary
                .as_file()
                .set_permissions(metadata.permissions())?;
        }
        temporary
            .write_all(content.as_bytes())
            .context("Failed to write configuration")?;
        temporary.flush().context("Failed to flush configuration")?;
        Ok(temporary)
    }

    fn persist_config(temporary: tempfile::NamedTempFile, path: &Path) -> Result<()> {
        // Editors may replace their file, so sync the current path rather than the original handle.
        File::open(temporary.path())?
            .sync_all()
            .context("Failed to sync configuration")?;
        temporary
            .persist(path)
            .context("Failed to replace configuration")?;
        File::open(config_parent(path))?
            .sync_all()
            .context("Failed to sync config directory")?;
        Ok(())
    }

    /// Get the default config file path (~/.config/autogit/config.toml)
    pub fn default_config_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir().context("Could not determine config directory")?;

        Ok(config_dir.join("autogit").join("config.toml"))
    }

    /// Load a configuration, creating it once when absent under the same transaction lock.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let _lock = Self::lock(path)?;
        if path.exists() {
            return Self::load(path);
        }
        let config = Self::default();
        config.save_atomic(path)?;
        Ok(config)
    }

    /// Load config from default location, or create a default one if it doesn't exist.
    pub fn load_or_create_default() -> Result<Self> {
        Self::load_or_create(Self::default_config_path()?)
    }
}

fn config_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
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
            repositories: vec![Repository {
                path: PathBuf::from("/home/user/notes"),
                auto_commit: true,
                commit_message_template: "Auto-commit: {timestamp}".to_owned(),
            }],
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
        assert!(repo.auto_commit); // default_true
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
        assert!(deserialized.repositories[0].auto_commit);
        assert!(!deserialized.repositories[1].auto_commit);
        assert_eq!(
            deserialized.repositories[2].commit_message_template,
            "Code changes"
        );
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

        assert!(!repo.auto_commit);
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
    #[test]
    fn invalid_interval_is_rejected_on_load_and_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = Config::default();
        config.save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        for interval in [0, u64::MAX] {
            config.daemon.check_interval_seconds = interval;
            assert!(config.save(&path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), original);
            std::fs::write(
                &path,
                format!("[daemon]\ncheck_interval_seconds = {interval}\n"),
            )
            .unwrap();
            assert!(Config::load(&path).is_err());
            std::fs::write(&path, &original).unwrap();
        }
        std::fs::write(&path, "").unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn replacement_preserves_old_open_file_and_permissions() {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = Config::default();
        config.save(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let mut old_file = File::open(&path).unwrap();
        config.daemon.check_interval_seconds = 42;
        config.save(&path).unwrap();
        let mut old_contents = String::new();
        old_file.read_to_string(&mut old_contents).unwrap();
        assert!(old_contents.contains("300"));
        assert_eq!(
            Config::load(&path).unwrap().daemon.check_interval_seconds,
            42
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn concurrent_transactions_preserve_every_update_and_readers_see_valid_files() {
        use std::sync::{
            Arc, Barrier,
            atomic::{AtomicBool, Ordering},
        };
        use std::thread;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let reader_path = path.clone();
        let reader_done = done.clone();
        let reader = thread::spawn(move || {
            while !reader_done.load(Ordering::Acquire) {
                Config::load(&reader_path).unwrap();
                thread::yield_now();
            }
        });
        let barrier = Arc::new(Barrier::new(4));
        let workers: Vec<_> = (0..4)
            .map(|worker| {
                let path = path.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    for index in 0..20 {
                        Config::update(&path, |config| {
                            config.repositories.push(Repository {
                                path: PathBuf::from(format!("/repository/{worker}/{index}")),
                                auto_commit: true,
                                commit_message_template: default_commit_message(),
                            });
                            Ok(())
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        done.store(true, Ordering::Release);
        reader.join().unwrap();
        assert_eq!(Config::load(&path).unwrap().repositories.len(), 80);
    }

    #[test]
    fn failed_transaction_leaves_original_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let result: Result<()> = Config::update(&path, |config| {
            config.repositories.clear();
            anyhow::bail!("Rejected update");
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
    #[test]
    fn editor_transaction_validates_private_copy_and_preserves_comments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(
            Config::edit_file(&path, |temporary| {
                std::fs::write(temporary, "[daemon]\ncheck_interval_seconds = 0\n")?;
                assert_eq!(std::fs::read(&path)?, original);
                Ok(())
            })
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        Config::edit_file(&path, |temporary| {
            let replacement = temporary.with_extension("replacement");
            std::fs::write(
                &replacement,
                "# Keep this comment\n[daemon]\ncheck_interval_seconds = 42\n",
            )?;
            std::fs::rename(replacement, temporary)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            Config::load(&path).unwrap().daemon.check_interval_seconds,
            42
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# Keep this comment")
        );
    }
}
