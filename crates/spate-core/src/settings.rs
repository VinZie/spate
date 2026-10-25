//! User settings, persisted as `settings.json` in [`crate::paths::data_dir`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::category::Category;
use crate::http::NetConfig;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Root folder for new downloads.
    pub download_dir: PathBuf,
    /// Save into `<download_dir>/<Category folder>` (Video, Music, ...).
    pub category_folders: bool,
    /// Default connections per download (1..=32).
    pub connections: usize,
    /// Downloads the queue runs at the same time.
    pub max_parallel: usize,
    /// Minimum segment size in KiB; segments under twice this are not split.
    pub min_split_kib: u64,
    /// Global speed limit in KiB/s, 0 = unlimited.
    pub speed_limit_kib: u64,
    pub max_retries: u32,
    pub net: NetConfig,
    /// Show a confirmation dialog for downloads captured from the browser.
    pub show_capture_dialog: bool,
    /// Offer to download URLs copied to the clipboard.
    pub clipboard_monitor: bool,
    /// Start processing the queue when the app launches.
    pub start_queue_on_launch: bool,
    /// Resume downloads that were active when the app last exited.
    pub resume_on_launch: bool,
    pub notify_on_complete: bool,
    pub schedule: Schedule,
    /// Look for a new version once a day.
    pub check_for_updates: bool,
    /// A version the user chose to skip; it is not offered again.
    pub skipped_version: Option<String>,
}

/// Daily window during which the queue runs automatically.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    pub enabled: bool,
    /// Local time `(hour, minute)` at which to start the queue.
    pub start: (u8, u8),
    /// Optional local time at which to stop it again.
    pub stop: Option<(u8, u8)>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: crate::paths::default_download_dir(),
            category_folders: true,
            connections: 8,
            max_parallel: 3,
            min_split_kib: 1024,
            speed_limit_kib: 0,
            max_retries: 8,
            net: NetConfig::default(),
            show_capture_dialog: true,
            clipboard_monitor: false,
            start_queue_on_launch: true,
            resume_on_launch: false,
            notify_on_complete: true,
            schedule: Schedule::default(),
            check_for_updates: true,
            skipped_version: None,
        }
    }
}

impl Settings {
    pub const FILE_NAME: &'static str = "settings.json";

    pub fn load(data_dir: &Path) -> Self {
        std::fs::read(data_dir.join(Self::FILE_NAME))
            .ok()
            .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
            .map(Settings::normalized)
            .unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        crate::paths::write_atomic(&data_dir.join(Self::FILE_NAME), &json)
    }

    /// Clamp values into supported ranges.
    pub fn normalized(mut self) -> Self {
        self.connections = self.connections.clamp(1, 32);
        self.max_parallel = self.max_parallel.clamp(1, 20);
        self.min_split_kib = self.min_split_kib.clamp(64, 1024 * 1024);
        self
    }

    /// Folder a new file of `category` is saved to by default.
    pub fn dir_for(&self, category: Category) -> PathBuf {
        if self.category_folders {
            self.download_dir.join(category.folder())
        } else {
            self.download_dir.clone()
        }
    }
}
