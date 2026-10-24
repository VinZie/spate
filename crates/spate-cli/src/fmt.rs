//! Human-readable sizes, rates and durations.

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if v >= 100.0 { format!("{v:.0} {}", UNITS[unit]) } else { format!("{v:.1} {}", UNITS[unit]) }
}

pub fn rate(bytes_per_sec: f64) -> String {
    format!("{}/s", bytes(bytes_per_sec.max(0.0) as u64))
}

/// `0:05`, `12:34`, `1:02:03`; `--:--` when unknown or absurd.
pub fn duration(secs: f64) -> String {
    if !(0.0..=99.0 * 3600.0).contains(&secs) {
        return "--:--".into();
    }
    let s = secs.round() as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Seconds with sensible precision for summaries (`0.84 s`, `12.3 s`, `2:05`).
pub fn elapsed(secs: f64) -> String {
    if secs < 10.0 {
        format!("{secs:.2} s")
    } else if secs < 60.0 {
        format!("{secs:.1} s")
    } else {
        duration(secs)
    }
}

/// An error and its causes: `error sending request: tcp connect error: ...`.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        let msg = s.to_string();
        if !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        cur = s.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(10 * 1024 * 1024), "10.0 MiB");
        assert_eq!(bytes(200 * 1024 * 1024), "200 MiB");
        assert_eq!(rate(2.0 * 1024.0 * 1024.0), "2.0 MiB/s");
        assert_eq!(duration(5.0), "0:05");
        assert_eq!(duration(754.0), "12:34");
        assert_eq!(duration(3723.0), "1:02:03");
        assert_eq!(duration(f64::INFINITY), "--:--");
        assert_eq!(elapsed(0.844), "0.84 s");
    }
}
