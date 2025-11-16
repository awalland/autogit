use anyhow::Result;
use ksni::{Icon, MenuItem, Tray};
use ksni::menu::*;
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
    pub start_time: Instant,
    pub last_sync: Option<Instant>,
    pub repo_count: usize,
    pub error_count: usize,
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
            start_time: Instant::now(),
            last_sync: None,
            repo_count,
            error_count: 0,
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

    /// Toggle suspend state
    pub fn toggle_suspend(&self) -> bool {
        let current = self.suspended.load(std::sync::atomic::Ordering::Relaxed);
        self.suspended.store(!current, std::sync::atomic::Ordering::Relaxed);
        !current
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

    /// Reset error count
    pub fn reset_errors(&self) {
        let mut state = self.status.write().unwrap();
        state.error_count = 0;
        if state.status == TrayStatus::Error {
            state.status = TrayStatus::Idle;
        }
    }

    /// Update repository count
    pub fn set_repo_count(&self, count: usize) {
        let mut state = self.status.write().unwrap();
        state.repo_count = count;
    }

    /// Spawn the tray service (using TrayMethods trait)
    pub async fn spawn_tray(self) -> Result<ksni::Handle<Self>> {
        use ksni::TrayMethods;
        // TrayMethods provides the .spawn() method
        let handle = TrayMethods::spawn(self).await?;
        info!("System tray icon spawned");
        Ok(handle)
    }

    /// Format uptime as human-readable string
    fn format_uptime(elapsed: std::time::Duration) -> String {
        let seconds = elapsed.as_secs();
        let hours = seconds / 3600;
        let minutes = (seconds % 3600) / 60;
        let secs = seconds % 60;

        if hours > 0 {
            format!("{}h {}m", hours, minutes)
        } else if minutes > 0 {
            format!("{}m {}s", minutes, secs)
        } else {
            format!("{}s", secs)
        }
    }

    /// Format time ago as human-readable string
    fn format_time_ago(instant: Instant) -> String {
        let elapsed = instant.elapsed().as_secs();

        if elapsed < 60 {
            format!("{} sec ago", elapsed)
        } else if elapsed < 3600 {
            format!("{} min ago", elapsed / 60)
        } else {
            format!("{} hr ago", elapsed / 3600)
        }
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
                TrayStatus::Idle => "Autogit Daemon".to_owned(),
                TrayStatus::Syncing => "Autogit - Syncing...".to_owned(),
                TrayStatus::Error => format!("Autogit - {} errors", state.error_count),
            }
        }
    }

    fn icon_name(&self) -> String {
        let state = self.get_state();
        let suspended = self.is_suspended();

        // Use standard icons that are more visible
        // Try symbolic first for theme awareness, with fallback to regular colored icons
        if suspended {
            "media-playback-pause"
        } else {
            match state.status {
                TrayStatus::Idle => "emblem-default",
                TrayStatus::Syncing => "view-refresh",
                TrayStatus::Error => "emblem-important",
            }
        }
        .to_owned()
    }

    fn icon_theme_path(&self) -> String {
        // Provide fallback icon theme path
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let state = self.get_state();
        let suspended = self.is_suspended();

        // Load icon with actual dimensions from PNG file
        let (width, height, data) = if suspended {
            create_pause_icon()
        } else {
            match state.status {
                TrayStatus::Idle => create_idle_icon(),
                TrayStatus::Syncing => create_syncing_icon(),
                TrayStatus::Error => create_error_icon(),
            }
        };

        // Provide multiple sizes for better display across different DPIs
        // System tray will choose the most appropriate size
        let mut icons = vec![Icon {
            width,
            height,
            data: data.clone(),
        }];

        // Also provide a 32x32 scaled version if original is larger
        if width > 32 || height > 32 {
            let (scaled_width, scaled_height, scaled_data) = scale_icon_to_32(&data, width, height);
            icons.push(Icon {
                width: scaled_width,
                height: scaled_height,
                data: scaled_data,
            });
        }

        icons
    }

    fn attention_icon_name(&self) -> String {
        String::new()
    }

    fn attention_icon_pixmap(&self) -> Vec<Icon> {
        let state = self.get_state();
        if state.error_count > 0 {
            let (width, height, data) = create_error_icon();
            let mut icons = vec![Icon {
                width,
                height,
                data: data.clone(),
            }];

            // Also provide 32x32 scaled version
            if width > 32 || height > 32 {
                let (scaled_width, scaled_height, scaled_data) = scale_icon_to_32(&data, width, height);
                icons.push(Icon {
                    width: scaled_width,
                    height: scaled_height,
                    data: scaled_data,
                });
            }

            icons
        } else {
            vec![]
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let state = self.get_state();
        let suspended = self.is_suspended();

        let error_text = if state.error_count > 0 {
            format!("⚠ {} errors", state.error_count)
        } else {
            "✓ No errors".to_owned()
        };

        let status_text = if suspended {
            "⏸ Suspended"
        } else {
            "✓ Active"
        };

        vec![
            // Repository info
            StandardItem {
                label: format!("Repositories: {}", state.repo_count),
                enabled: false,
                ..Default::default()
            }.into(),

            StandardItem {
                label: format!("Status: {}", status_text),
                enabled: false,
                ..Default::default()
            }.into(),

            StandardItem {
                label: error_text,
                enabled: false,
                ..Default::default()
            }.into(),

            MenuItem::Separator,

            // Actions
            StandardItem {
                label: if suspended { "▶ Resume" } else { "⏸ Suspend" }.into(),
                activate: Box::new(|this: &mut Self| {
                    let tx = this.trigger_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = tx.send(TrayAction::ToggleSuspend).await {
                            error!("Failed to send toggle suspend action: {}", e);
                        }
                    });
                }),
                ..Default::default()
            }.into(),

            StandardItem {
                label: "⚡ Sync Now".into(),
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
            }.into(),

            MenuItem::Separator,

            // Quit
            StandardItem {
                label: "🚪 Quit Daemon".into(),
                activate: Box::new(|this: &mut Self| {
                    let tx = this.trigger_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = tx.send(TrayAction::Quit).await {
                            error!("Failed to send quit action: {}", e);
                        }
                    });
                }),
                ..Default::default()
            }.into(),
        ]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        // Left-click on tray icon - could trigger sync or show status
        info!("Tray icon clicked");
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

    // Return dimensions and raw pixel data (RGBA bytes)
    (width, height, rgba.into_raw())
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

// Scale an icon to 32x32 for better tray compatibility
fn scale_icon_to_32(data: &[u8], width: i32, height: i32) -> (i32, i32, Vec<u8>) {
    use image::{RgbaImage, imageops::FilterType};

    // Create image from raw RGBA data
    let img = RgbaImage::from_raw(width as u32, height as u32, data.to_vec())
        .expect("Failed to create image from icon data");

    // Resize to 32x32 using high-quality Lanczos3 filter
    let resized = image::imageops::resize(&img, 32, 32, FilterType::Lanczos3);

    // Return dimensions and raw pixel data
    (32, 32, resized.into_raw())
}
