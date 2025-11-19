use anyhow::Result;
use autogit_shared::protocol::RepoDetail;
use ksni::{Icon, MenuItem, Tray};
use ksni::menu::*;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{info, error};

/// Tray icon state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayStatus {
    Idle,
    Syncing,
    Error,
}

/// Shared state for the system tray
pub struct AutogitTray {
    status: Arc<RwLock<TrayState>>,
    trigger_tx: mpsc::Sender<TrayAction>,
    suspended: Arc<std::sync::atomic::AtomicBool>,
}

/// Internal tray state
#[derive(Debug, Clone)]
struct TrayState {
    pub status: TrayStatus,
    pub last_sync: Option<Instant>,
    pub repo_count: usize,
    pub error_count: usize,
    pub repo_details: Vec<RepoDetail>,
    pub check_interval: u64,
}

/// Actions that can be triggered from the tray menu
#[derive(Debug, Clone)]
pub enum TrayAction {
    TriggerSync,
    ToggleSuspend,
    Quit,
}

impl AutogitTray {
    /// Create a new tray icon with action channel
    pub fn new(
        repo_count: usize,
        trigger_tx: mpsc::Sender<TrayAction>,
        suspended: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let state = TrayState {
            status: TrayStatus::Idle,
            last_sync: None,
            repo_count,
            error_count: 0,
            repo_details: Vec::new(),
            check_interval: 0,
        };

        Self {
            status: Arc::new(RwLock::new(state)),
            trigger_tx,
            suspended,
        }
    }

    /// Check if daemon is suspended
    pub fn is_suspended(&self) -> bool {
        self.suspended.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Update the tray status
    pub fn set_status(&self, status: TrayStatus) {
        let mut state = self.status.write().unwrap();
        state.status = status;
    }

    /// Update last sync time
    pub fn set_last_sync(&self) {
        let mut state = self.status.write().unwrap();
        state.last_sync = Some(Instant::now());
        state.status = TrayStatus::Idle;
    }

    /// Increment error count
    pub fn increment_errors(&self) {
        let mut state = self.status.write().unwrap();
        state.error_count += 1;
        state.status = TrayStatus::Error;
    }

    /// Update repository count
    pub fn set_repo_count(&self, count: usize) {
        let mut state = self.status.write().unwrap();
        state.repo_count = count;
    }

    /// Update repository details with sync results
    pub fn update_repo_details(&self, details: Vec<RepoDetail>) {
        let mut state = self.status.write().unwrap();
        state.repo_details = details;
        state.repo_count = state.repo_details.len();
    }

    /// Set the check interval in seconds
    pub fn set_check_interval(&self, interval: u64) {
        let mut state = self.status.write().unwrap();
        state.check_interval = interval;
    }

    /// Spawn the tray service (using TrayMethods trait)
    pub async fn spawn_tray(self) -> Result<ksni::Handle<Self>> {
        use ksni::TrayMethods;
        // TrayMethods provides the .spawn() method
        let handle = TrayMethods::spawn(self).await?;
        info!("System tray icon spawned");
        Ok(handle)
    }

    /// Get current state (for sync Tray trait)
    fn get_state(&self) -> TrayState {
        self.status.read().unwrap().clone()
    }
}

impl Tray for AutogitTray {
    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").to_owned()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        // Left-click on tray icon - could trigger sync or show status
        info!("Tray icon clicked");
    }

    fn category(&self) -> ksni::Category {
        // Use ApplicationStatus to prevent theme coloring
        ksni::Category::ApplicationStatus
    }

    fn title(&self) -> String {
        let state = self.get_state();
        let suspended = self.is_suspended();

        if suspended {
            "Autogit - Suspended".to_owned()
        } else {
            match state.status {
                TrayStatus::Idle => "Autogit".to_owned(),
                TrayStatus::Syncing => "Autogit - Syncing...".to_owned(),
                TrayStatus::Error => format!("Autogit - {} errors", state.error_count),
            }
        }
    }

    fn icon_theme_path(&self) -> String {
        // Provide fallback icon theme path
        String::new()
    }

    fn icon_name(&self) -> String {
        // Return empty string to force use of icon_pixmap (custom PNG icons)
        // If we return icon names, KDE will prefer theme icons over our custom ones
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let state = self.get_state();
        let suspended = self.is_suspended();

        // Render SVG at optimal size for system tray
        let (width, height, data) = if suspended {
            create_pause_icon()
        } else {
            match state.status {
                TrayStatus::Idle => create_idle_icon(),
                TrayStatus::Syncing => create_syncing_icon(),
                TrayStatus::Error => create_error_icon(),
            }
        };

        vec![Icon {
            width,
            height,
            data,
        }]
    }

    fn attention_icon_name(&self) -> String {
        String::new()
    }

    fn attention_icon_pixmap(&self) -> Vec<Icon> {
        let state = self.get_state();
        if state.error_count > 0 {
            let (width, height, data) = create_error_icon();
            vec![Icon {
                width,
                height,
                data,
            }]
        } else {
            vec![]
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let state = self.get_state();
        let suspended = self.is_suspended();

        let mut menu = Vec::new();

        // Daemon status
        let status_text = if suspended { "Suspended" } else { "Active" };
        menu.push(StandardItem {
            label: format!("Daemon status: {}", status_text),
            enabled: false,
            ..Default::default()
        }.into());

        // Check interval
        if state.check_interval > 0 {
            menu.push(StandardItem {
                label: format!("Check interval: {} seconds", state.check_interval),
                enabled: false,
                ..Default::default()
            }.into());
        }

        // Last sync time
        if let Some(last_sync) = state.last_sync {
            menu.push(StandardItem {
                label: format!("Last sync: {}", format_time_ago(last_sync)),
                enabled: false,
                ..Default::default()
            }.into());
        } else {
            menu.push(StandardItem {
                label: "Last sync: never".to_owned(),
                enabled: false,
                ..Default::default()
            }.into());
        }

        menu.push(MenuItem::Separator);

        // Repository list header
        menu.push(StandardItem {
            label: format!("Repositories ({})", state.repo_count),
            enabled: false,
            ..Default::default()
        }.into());

        // Individual repositories with status indicators
        if state.repo_details.is_empty() {
            // No details yet, just show count
            menu.push(StandardItem {
                label: "  (no sync data yet)".to_owned(),
                enabled: false,
                ..Default::default()
            }.into());
        } else {
            for repo in &state.repo_details {
                let icon = if repo.error.is_some() {
                    "✗"  // Error
                } else if repo.committed {
                    "✓"  // Success with commit
                } else {
                    "✓"  // Success without commit
                };

                let path = abbreviate_path(&repo.path);
                menu.push(StandardItem {
                    label: format!("  {} {}", icon, path),
                    enabled: false,
                    ..Default::default()
                }.into());
            }
        }

        menu.push(MenuItem::Separator);

        // Actions
        menu.push(StandardItem {
            label: "Sync Now".into(),
            enabled: !suspended,
            activate: Box::new(|this: &mut Self| {
                let tx = this.trigger_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = tx.send(TrayAction::TriggerSync).await {
                        error!("Failed to send trigger sync action: {}", e);
                    }
                });
            }),
            ..Default::default()
        }.into());

        menu.push(StandardItem {
            label: if suspended { "Resume" } else { "Suspend" }.into(),
            activate: Box::new(|this: &mut Self| {
                let tx = this.trigger_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = tx.send(TrayAction::ToggleSuspend).await {
                        error!("Failed to send toggle suspend action: {}", e);
                    }
                });
            }),
            ..Default::default()
        }.into());

        menu.push(MenuItem::Separator);

        // Quit
        menu.push(StandardItem {
            label: "Quit Autogit".into(),
            activate: Box::new(|this: &mut Self| {
                let tx = this.trigger_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = tx.send(TrayAction::Quit).await {
                        error!("Failed to send quit action: {}", e);
                    }
                });
            }),
            ..Default::default()
        }.into());

        menu
    }
}

// Helper functions for formatting display text

/// Abbreviate a path by replacing home directory with ~
fn abbreviate_path(path: &PathBuf) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(stripped) = path.strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
}

/// Format elapsed time since last sync in a human-readable way
fn format_time_ago(instant: Instant) -> String {
    let elapsed = instant.elapsed();
    let secs = elapsed.as_secs();

    if secs < 60 {
        "just now".to_owned()
    } else if secs < 3600 {
        let mins = secs / 60;
        if mins == 1 {
            "1 minute ago".to_owned()
        } else {
            format!("{} minutes ago", mins)
        }
    } else if secs < 86400 {
        let hours = secs / 3600;
        if hours == 1 {
            "1 hour ago".to_owned()
        } else {
            format!("{} hours ago", hours)
        }
    } else {
        let days = secs / 86400;
        if days == 1 {
            "1 day ago".to_owned()
        } else {
            format!("{} days ago", days)
        }
    }
}

// Helper functions to load icon pixel data from embedded PNG files
// Icons are loaded at compile time and decoded to RGBA format
// Returns (width, height, pixel_data)

fn load_png_icon(png_bytes: &[u8]) -> (i32, i32, Vec<u8>) {
    // Decode PNG image
    let img = image::load_from_memory(png_bytes)
        .expect("Failed to decode icon PNG");

    // Get dimensions
    let width = img.width() as i32;
    let height = img.height() as i32;

    // Convert to RGBA8 format (required by StatusNotifierItem)
    let rgba = img.to_rgba8();
    let data = rgba.into_raw();

    (width, height, data)
}

fn create_idle_icon() -> (i32, i32, Vec<u8>) {
    static ICON_BYTES: &[u8] = include_bytes!("../assets/icons/idle.png");
    load_png_icon(ICON_BYTES)
}

fn create_syncing_icon() -> (i32, i32, Vec<u8>) {
    static ICON_BYTES: &[u8] = include_bytes!("../assets/icons/syncing.png");
    load_png_icon(ICON_BYTES)
}

fn create_error_icon() -> (i32, i32, Vec<u8>) {
    static ICON_BYTES: &[u8] = include_bytes!("../assets/icons/error.png");
    load_png_icon(ICON_BYTES)
}

fn create_pause_icon() -> (i32, i32, Vec<u8>) {
    static ICON_BYTES: &[u8] = include_bytes!("../assets/icons/suspended.png");
    load_png_icon(ICON_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn create_test_tray() -> (AutogitTray, mpsc::Receiver<TrayAction>) {
        let (tx, rx) = mpsc::channel(10);
        let suspended = Arc::new(AtomicBool::new(false));
        let tray = AutogitTray::new(3, tx, suspended);
        (tray, rx)
    }

    #[test]
    fn test_new_tray_initial_state() {
        let (tray, _rx) = create_test_tray();
        let state = tray.get_state();

        assert_eq!(state.status, TrayStatus::Idle);
        assert_eq!(state.repo_count, 3);
        assert_eq!(state.error_count, 0);
        assert!(state.last_sync.is_none());
        assert!(state.repo_details.is_empty());
        assert_eq!(state.check_interval, 0);
        assert!(!tray.is_suspended());
    }

    #[test]
    fn test_set_status() {
        let (tray, _rx) = create_test_tray();

        tray.set_status(TrayStatus::Syncing);
        assert_eq!(tray.get_state().status, TrayStatus::Syncing);

        tray.set_status(TrayStatus::Error);
        assert_eq!(tray.get_state().status, TrayStatus::Error);

        tray.set_status(TrayStatus::Idle);
        assert_eq!(tray.get_state().status, TrayStatus::Idle);
    }

    #[test]
    fn test_set_last_sync() {
        let (tray, _rx) = create_test_tray();

        assert!(tray.get_state().last_sync.is_none());

        tray.set_last_sync();

        let state = tray.get_state();
        assert!(state.last_sync.is_some());
        assert_eq!(state.status, TrayStatus::Idle);
    }

    #[test]
    fn test_increment_errors() {
        let (tray, _rx) = create_test_tray();

        assert_eq!(tray.get_state().error_count, 0);

        tray.increment_errors();
        assert_eq!(tray.get_state().error_count, 1);
        assert_eq!(tray.get_state().status, TrayStatus::Error);

        tray.increment_errors();
        assert_eq!(tray.get_state().error_count, 2);
        assert_eq!(tray.get_state().status, TrayStatus::Error);
    }

    #[test]
    fn test_set_repo_count() {
        let (tray, _rx) = create_test_tray();

        assert_eq!(tray.get_state().repo_count, 3);

        tray.set_repo_count(5);
        assert_eq!(tray.get_state().repo_count, 5);

        tray.set_repo_count(0);
        assert_eq!(tray.get_state().repo_count, 0);
    }

    #[test]
    fn test_is_suspended() {
        let (tx, _rx) = mpsc::channel(10);
        let suspended = Arc::new(AtomicBool::new(false));
        let tray = AutogitTray::new(1, tx, suspended.clone());

        assert!(!tray.is_suspended());

        suspended.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(tray.is_suspended());

        suspended.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(!tray.is_suspended());
    }

    #[test]
    fn test_tray_title_idle() {
        let (tray, _rx) = create_test_tray();
        tray.set_status(TrayStatus::Idle);

        assert_eq!(tray.title(), "Autogit");
    }

    #[test]
    fn test_tray_title_syncing() {
        let (tray, _rx) = create_test_tray();
        tray.set_status(TrayStatus::Syncing);

        assert_eq!(tray.title(), "Autogit - Syncing...");
    }

    #[test]
    fn test_tray_title_error() {
        let (tray, _rx) = create_test_tray();
        tray.increment_errors();

        assert_eq!(tray.title(), "Autogit - 1 errors");

        tray.increment_errors();
        assert_eq!(tray.title(), "Autogit - 2 errors");
    }

    #[test]
    fn test_tray_title_suspended() {
        let (tx, _rx) = mpsc::channel(10);
        let suspended = Arc::new(AtomicBool::new(true));
        let tray = AutogitTray::new(1, tx, suspended);

        assert_eq!(tray.title(), "Autogit - Suspended");
    }

    #[test]
    fn test_tray_icon_name_always_empty() {
        let (tray, _rx) = create_test_tray();

        // icon_name should always be empty to force use of icon_pixmap
        assert_eq!(tray.icon_name(), "");

        tray.set_status(TrayStatus::Syncing);
        assert_eq!(tray.icon_name(), "");

        tray.increment_errors();
        assert_eq!(tray.icon_name(), "");
    }

    #[test]
    fn test_icon_pixmap_returns_valid_data() {
        let (tray, _rx) = create_test_tray();

        // Test idle icon
        tray.set_status(TrayStatus::Idle);
        let icons = tray.icon_pixmap();
        assert_eq!(icons.len(), 1);
        assert!(icons[0].width > 0);
        assert!(icons[0].height > 0);
        assert!(!icons[0].data.is_empty());

        // Test syncing icon
        tray.set_status(TrayStatus::Syncing);
        let icons = tray.icon_pixmap();
        assert_eq!(icons.len(), 1);
        assert!(icons[0].width > 0);

        // Test error icon
        tray.increment_errors();
        let icons = tray.icon_pixmap();
        assert_eq!(icons.len(), 1);
        assert!(icons[0].width > 0);
    }

    #[test]
    fn test_attention_icon_pixmap_with_errors() {
        let (tray, _rx) = create_test_tray();

        // No errors - should be empty
        let icons = tray.attention_icon_pixmap();
        assert_eq!(icons.len(), 0);

        // With errors - should return error icon
        tray.increment_errors();
        let icons = tray.attention_icon_pixmap();
        assert_eq!(icons.len(), 1);
        assert!(icons[0].width > 0);
        assert!(icons[0].height > 0);
        assert!(!icons[0].data.is_empty());
    }

    #[test]
    fn test_tray_category() {
        let (tray, _rx) = create_test_tray();
        assert_eq!(tray.category(), ksni::Category::ApplicationStatus);
    }

    #[test]
    fn test_tray_id() {
        let (tray, _rx) = create_test_tray();
        assert_eq!(tray.id(), env!("CARGO_PKG_NAME"));
    }

    #[test]
    fn test_load_png_icon() {
        // Test that icon loading works for all icon types
        let (width, height, data) = create_idle_icon();
        assert!(width > 0);
        assert!(height > 0);
        assert!(!data.is_empty());
        // RGBA format means 4 bytes per pixel
        assert_eq!(data.len(), (width * height * 4) as usize);

        let (width, height, data) = create_syncing_icon();
        assert!(width > 0);
        assert_eq!(data.len(), (width * height * 4) as usize);

        let (width, height, data) = create_error_icon();
        assert!(width > 0);
        assert_eq!(data.len(), (width * height * 4) as usize);

        let (width, height, data) = create_pause_icon();
        assert!(width > 0);
        assert_eq!(data.len(), (width * height * 4) as usize);
    }

    #[test]
    fn test_menu_items_count() {
        let (tray, _rx) = create_test_tray();
        tray.set_check_interval(120);
        let menu = tray.menu();

        // Expected menu structure:
        // - Daemon status: ...
        // - Check interval: ...
        // - Last sync: ...
        // - Separator
        // - Repositories (X)
        // - (no sync data yet)
        // - Separator
        // - Suspend/Resume
        // - Sync Now
        // - Separator
        // - Quit
        assert_eq!(menu.len(), 11);
    }

    #[test]
    fn test_menu_suspend_resume_label() {
        // Test when not suspended
        let (tx, _rx) = mpsc::channel(10);
        let suspended = Arc::new(AtomicBool::new(false));
        let tray = AutogitTray::new(1, tx.clone(), suspended.clone());
        let menu = tray.menu();

        // Find the suspend/resume item in menu (not at fixed index anymore)
        let suspend_item = menu.iter().find(|item| {
            if let MenuItem::Standard(ref std_item) = item {
                std_item.label == "Suspend" || std_item.label == "Resume"
            } else {
                false
            }
        });

        assert!(suspend_item.is_some());
        if let Some(MenuItem::Standard(ref item)) = suspend_item {
            assert_eq!(item.label, "Suspend");
        }

        // Test when suspended
        suspended.store(true, std::sync::atomic::Ordering::Relaxed);
        let menu = tray.menu();
        let suspend_item = menu.iter().find(|item| {
            if let MenuItem::Standard(ref std_item) = item {
                std_item.label == "Suspend" || std_item.label == "Resume"
            } else {
                false
            }
        });

        if let Some(MenuItem::Standard(ref item)) = suspend_item {
            assert_eq!(item.label, "Resume");
        }
    }

    #[test]
    fn test_menu_sync_now_disabled_when_suspended() {
        // Test when not suspended
        let (tx, _rx) = mpsc::channel(10);
        let suspended = Arc::new(AtomicBool::new(false));
        let tray = AutogitTray::new(1, tx.clone(), suspended.clone());
        let menu = tray.menu();

        // Find "Sync Now" item in menu (not at fixed index anymore)
        let sync_now_item = menu.iter().find(|item| {
            if let MenuItem::Standard(ref std_item) = item {
                std_item.label == "Sync Now"
            } else {
                false
            }
        });
        assert!(sync_now_item.is_some());
        if let Some(MenuItem::Standard(ref item)) = sync_now_item {
            assert!(item.enabled);
        }

        // Test when suspended
        suspended.store(true, std::sync::atomic::Ordering::Relaxed);
        let menu = tray.menu();
        let sync_now_item = menu.iter().find(|item| {
            if let MenuItem::Standard(ref std_item) = item {
                std_item.label == "Sync Now"
            } else {
                false
            }
        });
        if let Some(MenuItem::Standard(ref item)) = sync_now_item {
            assert!(!item.enabled);
        }
    }

    #[test]
    fn test_update_repo_details() {
        use std::path::PathBuf;
        let (tray, _rx) = create_test_tray();

        let details = vec![
            RepoDetail {
                path: PathBuf::from("/home/user/repo1"),
                committed: true,
                files_changed: None,
                error: None,
            },
            RepoDetail {
                path: PathBuf::from("/home/user/repo2"),
                committed: false,
                files_changed: None,
                error: Some("Test error".to_owned()),
            },
        ];

        tray.update_repo_details(details.clone());

        let state = tray.get_state();
        assert_eq!(state.repo_details.len(), 2);
        assert_eq!(state.repo_count, 2);
        assert_eq!(state.repo_details[0].path, PathBuf::from("/home/user/repo1"));
        assert!(state.repo_details[0].committed);
        assert!(state.repo_details[0].error.is_none());
        assert_eq!(state.repo_details[1].path, PathBuf::from("/home/user/repo2"));
        assert!(!state.repo_details[1].committed);
        assert!(state.repo_details[1].error.is_some());
    }

    #[test]
    fn test_set_check_interval() {
        let (tray, _rx) = create_test_tray();

        assert_eq!(tray.get_state().check_interval, 0);

        tray.set_check_interval(120);
        assert_eq!(tray.get_state().check_interval, 120);

        tray.set_check_interval(60);
        assert_eq!(tray.get_state().check_interval, 60);
    }

    #[test]
    fn test_menu_displays_check_interval() {
        let (tray, _rx) = create_test_tray();
        tray.set_check_interval(120);

        let menu = tray.menu();

        // Find the check interval item
        let interval_item = menu.iter().find(|item| {
            if let MenuItem::Standard(ref std_item) = item {
                std_item.label.contains("Check interval")
            } else {
                false
            }
        });

        assert!(interval_item.is_some());
        if let Some(MenuItem::Standard(ref item)) = interval_item {
            assert_eq!(item.label, "Check interval: 120 seconds");
            assert!(!item.enabled);
        }
    }

    #[test]
    fn test_menu_displays_repo_details() {
        use std::path::PathBuf;
        let (tray, _rx) = create_test_tray();

        let details = vec![
            RepoDetail {
                path: PathBuf::from("/home/user/repo1"),
                committed: true,
                files_changed: None,
                error: None,
            },
            RepoDetail {
                path: PathBuf::from("/home/user/repo2"),
                committed: false,
                files_changed: None,
                error: Some("Test error".to_owned()),
            },
        ];

        tray.update_repo_details(details);

        let menu = tray.menu();

        // Should contain repository paths with status indicators
        let repo_items: Vec<_> = menu.iter().filter(|item| {
            if let MenuItem::Standard(ref std_item) = item {
                std_item.label.contains("/home/user/repo")
            } else {
                false
            }
        }).collect();

        assert_eq!(repo_items.len(), 2);

        // Check for success/error indicators
        if let Some(MenuItem::Standard(ref item)) = repo_items.get(0) {
            assert!(item.label.contains("✓"));
        }
        if let Some(MenuItem::Standard(ref item)) = repo_items.get(1) {
            assert!(item.label.contains("✗"));
        }
    }

    #[test]
    fn test_abbreviate_path() {
        use std::path::PathBuf;

        if let Some(home) = dirs::home_dir() {
            let test_path = home.join("test").join("repo");
            let abbreviated = abbreviate_path(&test_path);
            assert!(abbreviated.starts_with("~/"));
            assert!(abbreviated.contains("test/repo"));
        }

        // Path outside home should not be abbreviated
        let abs_path = PathBuf::from("/opt/test/repo");
        let not_abbreviated = abbreviate_path(&abs_path);
        assert_eq!(not_abbreviated, "/opt/test/repo");
    }

    #[test]
    fn test_format_time_ago() {
        use std::time::Duration;

        // Just now
        let instant = Instant::now();
        assert_eq!(format_time_ago(instant), "just now");

        // 1 minute ago
        let instant = Instant::now() - Duration::from_secs(65);
        assert_eq!(format_time_ago(instant), "1 minute ago");

        // Multiple minutes ago
        let instant = Instant::now() - Duration::from_secs(180);
        assert_eq!(format_time_ago(instant), "3 minutes ago");

        // 1 hour ago
        let instant = Instant::now() - Duration::from_secs(3700);
        assert_eq!(format_time_ago(instant), "1 hour ago");

        // Multiple hours ago
        let instant = Instant::now() - Duration::from_secs(7200);
        assert_eq!(format_time_ago(instant), "2 hours ago");

        // 1 day ago
        let instant = Instant::now() - Duration::from_secs(86500);
        assert_eq!(format_time_ago(instant), "1 day ago");

        // Multiple days ago
        let instant = Instant::now() - Duration::from_secs(172800);
        assert_eq!(format_time_ago(instant), "2 days ago");
    }
}
