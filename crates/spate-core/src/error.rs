use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("disk error: {0}")]
    Io(#[from] std::io::Error),
    #[error("server responded with HTTP {0}")]
    Status(u16),
    /// 429/503: the server limits connections or request rate.
    #[error("server is limiting connections (HTTP {status})")]
    Throttled { status: u16, retry_after: Option<std::time::Duration> },
    #[error("the file changed on the server since the download started")]
    RemoteChanged,
    #[error("no data received for {0} seconds")]
    Stalled(u64),
    #[error("connection closed before the segment was complete")]
    Truncated,
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    Checksum { expected: String, actual: String },
    /// The size the server declared doesn't fit on the target volume.
    #[error("not enough disk space: the file needs {} but only {} are free", size(.needed), size(.available))]
    DiskFull { needed: u64, available: u64 },
    #[error("{0}")]
    Other(String),
}

/// `1.5 GiB`-style size for messages.
fn size(bytes: &u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = *bytes as f64;
    let mut unit = None;
    for u in UNITS {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = Some(u);
    }
    match unit {
        Some(u) => format!("{value:.1} {u}"),
        None => format!("{bytes} bytes"),
    }
}

impl Error {
    /// Errors that a retry on a fresh connection may fix.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Http(e) => !e.is_builder() && !e.is_redirect(),
            Error::Status(code) => crate::http::is_retryable_status(*code),
            Error::Stalled(_) | Error::Truncated | Error::Throttled { .. } => true,
            _ => false,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
