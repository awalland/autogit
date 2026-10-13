use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::{Builder, NamedTempFile};

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

struct ConfigPath {
    target: PathBuf,
    watch_paths: Vec<PathBuf>,
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
        let path = Self::resolved_path(path)?;
        let _lock = Self::lock(&path)?;
        self.save_atomic(&path)
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
        let path = Self::resolved_path(path)?;
        let _lock = Self::lock(&path)?;
        let mut config = if path.exists() {
            Self::load(&path)?
        } else {
            Self::default()
        };
        let result = update(&mut config)?;
        config.validate()?;
        config.save_atomic(&path)?;
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

    /// Resolve the stable persistence target, preserving symlinks when replacing a config.
    /// Missing target components are allowed so a dangling link can create its target.
    pub fn resolved_path(path: impl AsRef<Path>) -> Result<PathBuf> {
        Ok(Self::resolve_path(path.as_ref())?.target)
    }

    /// Entries whose replacement can change the config contents or symlink target.
    pub fn watch_paths(path: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        Ok(Self::resolve_path(path.as_ref())?.watch_paths)
    }

    fn resolve_path(path: &Path) -> Result<ConfigPath> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut watch_paths = vec![absolute.clone()];
        let mut remaining: VecDeque<_> = absolute
            .components()
            .map(|component| component.as_os_str().to_os_string())
            .collect();
        let mut resolved = PathBuf::new();
        let mut links = 0;
        while let Some(part) = remaining.pop_front() {
            let component = Path::new(&part).components().next().unwrap();
            match component {
                Component::RootDir => resolved.push(component.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Normal(_) => {
                    let candidate = resolved.join(&part);
                    match std::fs::symlink_metadata(&candidate) {
                        Ok(metadata) if metadata.file_type().is_symlink() => {
                            watch_paths.push(candidate.clone());
                            links += 1;
                            ensure!(links <= 40, "Too many configuration path symlinks");
                            let target = std::fs::read_link(&candidate)?;
                            if target.is_absolute() {
                                resolved.clear();
                            }
                            for component in target.components().rev() {
                                remaining.push_front(component.as_os_str().to_os_string());
                            }
                        }
                        Ok(_) => resolved.push(part),
                        Err(error) if error.kind() == ErrorKind::NotFound => resolved.push(part),
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("Failed to resolve config path: {}", candidate.display())
                            });
                        }
                    }
                }
                Component::Prefix(_) => resolved.push(component.as_os_str()),
            }
        }
        ensure!(
            resolved.file_name().is_some(),
            "Config path has no filename"
        );
        watch_paths.push(resolved.clone());
        watch_paths.sort();
        watch_paths.dedup();
        Ok(ConfigPath {
            target: resolved,
            watch_paths,
        })
    }

    /// Edit without holding the transaction lock, rejecting concurrent changes at publication.
    pub fn edit_file(path: impl AsRef<Path>, edit: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
        let requested_path = path.as_ref();
        let path = Self::resolved_path(requested_path)?;
        std::fs::create_dir_all(config_parent(&path))?;
        let original = Self::read_optional(&path)?;
        let content = match &original {
            Some(content) => content.clone(),
            None => toml::to_string_pretty(&Self::default())?,
        };
        let temporary = Self::temporary_config(&path, &content)?;
        let edited = edit(temporary.path())
            .and_then(|()| Self::load(temporary.path()).context("Edited configuration is invalid"));
        if let Err(error) = edited {
            return Err(Self::retain_edit(temporary, error));
        }
        let publication = (|| {
            let lock = Self::lock(&path)?;
            ensure!(
                Self::resolved_path(requested_path)? == path,
                "Configuration target changed while the editor was open; edit was not applied"
            );
            ensure!(
                Self::read_optional(&path)? == original,
                "Configuration changed while the editor was open; edit was not applied"
            );
            Ok(lock)
        })();
        match publication {
            Ok(_lock) => Self::persist_config(temporary, &path),
            Err(error) => Err(Self::retain_edit(temporary, error)),
        }
    }

    fn read_optional(path: &Path) -> Result<Option<String>> {
        match std::fs::read_to_string(path) {
            Ok(content) => Ok(Some(content)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("Failed to read configuration"),
        }
    }

    fn retain_edit(temporary: NamedTempFile, error: anyhow::Error) -> anyhow::Error {
        match temporary.keep() {
            Ok((_, path)) => error.context(format!(
                "Edited configuration retained at {}",
                path.display()
            )),
            Err(retention) => error.context(format!(
                "Failed to retain edited configuration: {retention}"
            )),
        }
    }

    fn save_atomic(&self, path: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self).context("Failed to serialize config")?;
        Self::persist_config(Self::temporary_config(path, &content)?, path)
    }

    fn temporary_config(path: &Path, content: &str) -> Result<NamedTempFile> {
        let mut temporary = Builder::new()
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

    fn persist_config(temporary: NamedTempFile, path: &Path) -> Result<()> {
        // Editors may replace their file, so sync the current path rather than the original handle.
        if let Err(error) = File::open(temporary.path())
            .and_then(|file| file.sync_all())
            .context("Failed to sync configuration")
        {
            return Err(Self::retain_edit(temporary, error));
        }
        temporary.persist(path).map_err(|error| {
            Self::retain_edit(error.file, error.error.into())
                .context("Failed to replace configuration")
        })?;
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
        let path = Self::resolved_path(path)?;
        if path.exists() {
            return Self::load(&path);
        }
        let _lock = Self::lock(&path)?;
        if path.exists() {
            return Self::load(&path);
        }
        let config = Self::default();
        config.save_atomic(&path)?;
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
    #[test]
    fn atomic_writes_preserve_multihop_symlinks_and_create_missing_targets() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let alias = directory.path().join("config.toml");
        let intermediate = directory.path().join("next.toml");
        let target = directory.path().join("dotfiles/new/config.toml");
        symlink("next.toml", &alias).unwrap();
        symlink("dotfiles/new/config.toml", &intermediate).unwrap();
        assert_eq!(Config::resolved_path(&alias).unwrap(), target);
        let watched = Config::watch_paths(&alias).unwrap();
        assert!(watched.contains(&alias));
        assert!(watched.contains(&intermediate));
        assert!(watched.contains(&target));
        Config::load_or_create(&alias).unwrap();
        Config::update(&alias, |config| {
            config.daemon.check_interval_seconds = 42;
            Ok(())
        })
        .unwrap();
        assert!(
            std::fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            std::fs::symlink_metadata(&intermediate)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            Config::load(&target).unwrap().daemon.check_interval_seconds,
            42
        );
        assert!(target.with_file_name("config.toml.lock").exists());
        assert!(!alias.with_file_name("config.toml.lock").exists());
    }

    #[test]
    fn parent_symlink_and_parent_traversal_resolve_to_target_identity() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let actual = directory.path().join("actual/nested");
        std::fs::create_dir_all(&actual).unwrap();
        let alias = directory.path().join("alias");
        symlink("actual/nested", &alias).unwrap();
        assert_eq!(
            Config::resolved_path(alias.join("../config.toml")).unwrap(),
            directory.path().join("actual/config.toml")
        );
        symlink("loop", directory.path().join("loop")).unwrap();
        assert!(Config::resolved_path(directory.path().join("loop")).is_err());
    }

    #[test]
    fn transactions_through_aliases_share_one_lock_and_preserve_updates() {
        use std::os::unix::fs::symlink;
        use std::sync::{Arc, Barrier};
        use std::thread;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("config.toml");
        Config::default().save(&target).unwrap();
        let alias = directory.path().join("alias.toml");
        symlink("config.toml", &alias).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let workers: Vec<_> = [target.clone(), alias]
            .into_iter()
            .map(|path| {
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    for index in 0..20 {
                        Config::update(&path, |config| {
                            config.repositories.push(Repository {
                                path: PathBuf::from(format!("/repository/{index}")),
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
        assert_eq!(Config::load(&target).unwrap().repositories.len(), 40);
    }

    #[test]
    fn open_editor_allows_reads_and_updates_and_retains_conflicting_edit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let mut recovery = None;
        let error = Config::edit_file(&path, |temporary| {
            recovery = Some(temporary.to_path_buf());
            assert_eq!(
                Config::load_or_create(&path)?.daemon.check_interval_seconds,
                300
            );
            Config::update(&path, |config| {
                config.daemon.check_interval_seconds = 60;
                Ok(())
            })?;
            std::fs::write(temporary, "[daemon]\ncheck_interval_seconds = 42\n")?;
            Ok(())
        })
        .unwrap_err();
        let recovery = recovery.unwrap();
        let error = format!("{error:#}");
        assert!(error.contains("Configuration changed while the editor was open"));
        assert!(error.contains(recovery.to_str().unwrap()));
        assert_eq!(
            Config::load(&path).unwrap().daemon.check_interval_seconds,
            60
        );
        assert_eq!(
            Config::load(&recovery)
                .unwrap()
                .daemon
                .check_interval_seconds,
            42
        );
    }

    #[test]
    fn startup_reads_existing_config_without_waiting_for_transaction_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let _lock = Config::lock(&path).unwrap();
        assert_eq!(
            Config::load_or_create(&path)
                .unwrap()
                .daemon
                .check_interval_seconds,
            300
        );
    }

    #[test]
    fn editor_rejects_concurrent_creation_and_retains_invalid_edit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let error = Config::edit_file(&path, |temporary| {
            Config::default().save(&path)?;
            std::fs::write(temporary, "[daemon]\ncheck_interval_seconds = 42\n")?;
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("Configuration changed"));
        let mut recovery = None;
        let error = Config::edit_file(&path, |temporary| {
            recovery = Some(temporary.to_path_buf());
            std::fs::write(temporary, "invalid toml")?;
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("Edited configuration is invalid"));
        assert_eq!(
            std::fs::read_to_string(recovery.unwrap()).unwrap(),
            "invalid toml"
        );
    }
    #[test]
    fn editor_rejects_retargeted_symlink() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original.toml");
        let other = directory.path().join("other.toml");
        let alias = directory.path().join("config.toml");
        Config::default().save(&original).unwrap();
        Config::default().save(&other).unwrap();
        symlink("original.toml", &alias).unwrap();
        let error = Config::edit_file(&alias, |temporary| {
            std::fs::write(temporary, "[daemon]\ncheck_interval_seconds = 42\n")?;
            std::fs::remove_file(&alias)?;
            symlink("other.toml", &alias)?;
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("Configuration target changed"));
        assert_eq!(
            Config::load(&original)
                .unwrap()
                .daemon
                .check_interval_seconds,
            300
        );
        assert_eq!(
            Config::load(&other).unwrap().daemon.check_interval_seconds,
            300
        );
    }
}
