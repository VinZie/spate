//! Spate core: a fast multi-connection download engine plus the download
//! manager, persistence and IPC used by the desktop app and the CLI.

pub mod category;
pub mod download;
pub mod error;
pub mod fileio;
pub mod hls;
pub mod http;
pub mod ipc;
pub mod limiter;
pub mod manager;
pub mod naming;
pub mod paths;
pub mod segments;
pub mod settings;

pub use category::Category;
pub use download::{DownloadRequest, EngineOptions, Outcome, Progress, ResumeData};
pub use error::{Error, Result};
pub use limiter::RateLimiter;
pub use manager::{AddRequest, DownloadKind, ItemView, Manager, ProbeInfo, StartMode, Status};
pub use settings::Settings;
