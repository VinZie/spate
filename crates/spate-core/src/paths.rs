//! Well-known locations.

use std::path::PathBuf;

/// Per-user data directory (`%LOCALAPPDATA%\Spate` on Windows). Holds the
/// download list, settings and the IPC endpoint file. Overridable with the
/// `SPATE_DATA_DIR` environment variable (used by tests).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SPATE_DATA_DIR") {
        return PathBuf::from(dir);
    }
    directories::ProjectDirs::from("", "", "Spate")
        .map(|d| d.data_local_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("Spate"))
}

/// The user's Downloads folder (falls back to the home directory).
pub fn default_download_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(PathBuf::from).or_else(|| Some(u.home_dir().join("Downloads"))))
        .unwrap_or_else(|| std::env::temp_dir().join("Downloads"))
}

/// Write `contents` to `path` atomically (write a sibling temp file, then rename).
pub fn write_atomic(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = crate::naming::with_suffix(path, ".tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}
