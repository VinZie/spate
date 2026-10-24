//! Human-readable sizes, speeds, durations and dates.

use std::time::Duration;

const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];

/// `1536` -> `"1.50 KB"`. Binary multiples, labelled the way Windows does.
pub fn size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    // Keep three significant digits; avoid "1024 KB" by rounding up a unit.
    if value >= 999.5 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{} {}", three_digits(value), UNITS[unit])
}

fn three_digits(v: f64) -> String {
    if v < 9.995 {
        format!("{v:.2}")
    } else if v < 99.95 {
        format!("{v:.1}")
    } else {
        format!("{v:.0}")
    }
}

/// Size, or a dash when unknown.
pub fn opt_size(bytes: Option<u64>) -> String {
    bytes.map(size).unwrap_or_else(|| "–".into())
}

/// Transfer rate; empty when idle.
pub fn speed(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec < 1.0 {
        return String::new();
    }
    format!("{}/s", size(bytes_per_sec as u64))
}

/// `"12.0 MB of 40.0 MB"`, or just the downloaded amount when the total is unknown.
pub fn progress_of(downloaded: u64, total: Option<u64>) -> String {
    match total {
        Some(t) => format!("{} of {}", size(downloaded), size(t)),
        None => size(downloaded),
    }
}

/// Time remaining: `"8s"`, `"3m 05s"`, `"2h 07m"`, `"4d 03h"`.
pub fn eta(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else if s < 86_400 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else if s < 100 * 86_400 {
        format!("{}d {:02}h", s / 86_400, (s % 86_400) / 3600)
    } else {
        "> 99d".into()
    }
}

/// `0.4237` -> `"42.4%"`, `1.0` -> `"100%"`.
pub fn percent(fraction: f32) -> String {
    let p = (fraction.clamp(0.0, 1.0) * 100.0) as f64;
    if p >= 100.0 {
        "100%".into()
    } else {
        // Never show 100% for an unfinished download.
        let shown = (p * 10.0).floor() / 10.0;
        format!("{shown:.1}%")
    }
}

/// Unix seconds -> local `"2026-10-24 13:05"`.
pub fn date_time(unix_secs: u64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(unix_secs as i64, 0) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => t.format("%Y-%m-%d %H:%M").to_string(),
        chrono::LocalResult::None => String::new(),
    }
}

/// Compact local date for table cells: time only for today, else the date.
pub fn short_date(unix_secs: u64, now_unix: u64) -> String {
    use chrono::{Local, TimeZone};
    let (Some(t), Some(now)) = (
        Local.timestamp_opt(unix_secs as i64, 0).earliest(),
        Local.timestamp_opt(now_unix as i64, 0).earliest(),
    ) else {
        return String::new();
    };
    if t.date_naive() == now.date_naive() {
        t.format("Today %H:%M").to_string()
    } else {
        t.format("%Y-%m-%d").to_string()
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Speed limit label for KiB/s settings (0 = unlimited).
pub fn limit_kib(kib: u64) -> String {
    if kib == 0 { "Unlimited".into() } else { speed(kib as f64 * 1024.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1024), "1.00 KB");
        assert_eq!(size(1536), "1.50 KB");
        assert_eq!(size(10 * 1024 + 512), "10.5 KB");
        assert_eq!(size(500 * 1024), "500 KB");
        assert_eq!(size(1023 * 1024), "1.00 MB");
        assert_eq!(size(5 * 1024 * 1024), "5.00 MB");
        assert_eq!(size(3 * 1024 * 1024 * 1024), "3.00 GB");
        assert_eq!(size(u64::MAX), "16.0 EB");
        assert_eq!(opt_size(None), "–");
    }

    #[test]
    fn speeds() {
        assert_eq!(speed(0.0), "");
        assert_eq!(speed(f64::NAN), "");
        assert_eq!(speed(2048.0), "2.00 KB/s");
        assert_eq!(speed(12.5 * 1024.0 * 1024.0), "12.5 MB/s");
        assert_eq!(limit_kib(0), "Unlimited");
        assert_eq!(limit_kib(512), "512 KB/s");
    }

    #[test]
    fn etas() {
        assert_eq!(eta(Duration::from_secs(0)), "0s");
        assert_eq!(eta(Duration::from_secs(59)), "59s");
        assert_eq!(eta(Duration::from_secs(65)), "1m 05s");
        assert_eq!(eta(Duration::from_secs(7260)), "2h 01m");
        assert_eq!(eta(Duration::from_secs(90_000)), "1d 01h");
        assert_eq!(eta(Duration::from_secs(1_000 * 86_400)), "> 99d");
    }

    #[test]
    fn percents() {
        assert_eq!(percent(0.0), "0.0%");
        assert_eq!(percent(0.4237), "42.3%");
        assert_eq!(percent(0.99999), "99.9%");
        assert_eq!(percent(1.0), "100%");
        assert_eq!(percent(2.0), "100%");
    }

    #[test]
    fn progress_text() {
        assert_eq!(progress_of(1024, Some(4096)), "1.00 KB of 4.00 KB");
        assert_eq!(progress_of(1024, None), "1.00 KB");
    }

    #[test]
    fn dates() {
        let s = date_time(1_700_000_000);
        assert_eq!(s.len(), 16, "{s}");
        assert!(short_date(1_700_000_000, 1_700_000_000).starts_with("Today"));
        assert!(!short_date(1_600_000_000, 1_700_000_000).starts_with("Today"));
    }
}
