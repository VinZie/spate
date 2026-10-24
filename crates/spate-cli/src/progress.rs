//! Live single-line progress display on stderr.

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use spate_core::download::{Phase, Progress};
use spate_core::segments::SegmentView;

use crate::fmt;

/// Window over which the current speed is averaged.
const SPEED_WINDOW: Duration = Duration::from_secs(3);
/// Interval between progress lines when stderr is not a terminal.
const PLAIN_INTERVAL: Duration = Duration::from_secs(5);

pub struct Display {
    quiet: bool,
    tty: bool,
    last_len: usize,
    samples: VecDeque<(Instant, u64)>,
    started: Instant,
    /// Bytes already on disk when this run started (resumed downloads).
    base: u64,
    announced: bool,
    last_plain: Option<Instant>,
}

impl Display {
    pub fn new(quiet: bool) -> Self {
        Self {
            quiet,
            tty: std::io::stderr().is_terminal(),
            last_len: 0,
            samples: VecDeque::new(),
            started: Instant::now(),
            base: 0,
            announced: false,
            // Without a terminal, the first status line comes after a while.
            last_plain: Some(Instant::now()),
        }
    }

    /// Current speed in bytes/s over the last few seconds.
    pub fn speed(&self) -> f64 {
        match (self.samples.front(), self.samples.back()) {
            (Some(a), Some(b)) if b.0 > a.0 => (b.1.saturating_sub(a.1)) as f64 / (b.0 - a.0).as_secs_f64(),
            _ => 0.0,
        }
    }

    /// Declare bytes that were already downloaded before this run.
    pub fn set_base(&mut self, bytes: u64) {
        self.base = bytes;
    }

    /// Average speed of this run (excluding bytes that were already present).
    pub fn average(&self, received: u64) -> f64 {
        let secs = self.started.elapsed().as_secs_f64().max(1e-3);
        received.saturating_sub(self.base) as f64 / secs
    }

    pub fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    pub fn update(&mut self, p: &Progress) {
        let now = Instant::now();
        let received = p.received();
        let info = p.info();
        self.samples.push_back((now, received));
        while self.samples.len() > 2 && self.samples.front().is_some_and(|s| now - s.0 > SPEED_WINDOW) {
            self.samples.pop_front();
        }
        if self.quiet {
            return;
        }
        if let Some(info) = &info
            && !self.announced
        {
            self.announced = true;
            let size = info.total.map(fmt::bytes).unwrap_or_else(|| "unknown size".into());
            let resumable = if info.resumable { "resumable" } else { "not resumable" };
            self.println(&format!("{} ({size}, {resumable}) -> {}", info.file_name, info.file_path.display()));
        }

        let line = self.line(p, info.as_ref().and_then(|i| i.total), received);
        if self.tty {
            self.overwrite(&line);
        } else if self.last_plain.is_none_or(|t| now - t >= PLAIN_INTERVAL) {
            self.last_plain = Some(now);
            eprintln!("{}", line.trim());
        }
    }

    fn line(&self, p: &Progress, total: Option<u64>, received: u64) -> String {
        let segments = if self.tty { p.segments() } else { Vec::new() };
        let width = if self.tty { terminal_width().unwrap_or(80).clamp(20, 400) - 1 } else { 0 };
        render_line(&Snapshot {
            phase: p.phase(),
            total,
            received,
            speed: self.speed(),
            connections: p.active_connections(),
            segments: &segments,
            width,
        })
    }

    fn overwrite(&mut self, line: &str) {
        let width = terminal_width().unwrap_or(80).clamp(20, 400) - 1;
        let line: String = line.chars().take(width).collect();
        let len = line.chars().count();
        let pad = self.last_len.saturating_sub(len);
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r{line}{}", " ".repeat(pad));
        let _ = err.flush();
        self.last_len = len;
    }

    /// Erase the progress line.
    pub fn clear(&mut self) {
        if self.tty && self.last_len > 0 {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r{}\r", " ".repeat(self.last_len));
            let _ = err.flush();
            self.last_len = 0;
        }
    }

    /// Print a message on its own line, keeping the progress line intact.
    pub fn println(&mut self, msg: &str) {
        self.clear();
        eprintln!("{msg}");
    }

    pub fn done(&mut self, path: &Path, size: u64) {
        self.clear();
        if !self.quiet {
            let secs = self.elapsed();
            let fetched = if self.base > 0 {
                format!("{} total, {} fetched now", fmt::bytes(size), fmt::bytes(size.saturating_sub(self.base)))
            } else {
                fmt::bytes(size)
            };
            eprintln!(
                "Saved {} ({fetched} in {}, {} average)",
                path.display(),
                fmt::elapsed(secs),
                fmt::rate(self.average(size))
            );
        }
    }
}

/// Everything one progress line shows.
pub struct Snapshot<'a> {
    pub phase: Phase,
    pub total: Option<u64>,
    pub received: u64,
    pub speed: f64,
    pub connections: usize,
    pub segments: &'a [SegmentView],
    /// Terminal columns available; 0 = no terminal (no segment bar).
    pub width: usize,
}

/// `42.0% [####==--....] 4.2 MiB / 10.0 MiB  1.0 MiB/s  ETA 0:06  8 conn`
pub fn render_line(s: &Snapshot) -> String {
    match s.phase {
        Phase::Connecting => return "Connecting...".into(),
        Phase::Finishing => return "Finishing...".into(),
        Phase::Verifying => return "Verifying SHA-256...".into(),
        Phase::Downloading => {}
    }
    let conns = s.connections;
    match s.total {
        Some(total) if total > 0 => {
            let pct = s.received as f64 * 100.0 / total as f64;
            let eta = if s.speed > 0.0 { total.saturating_sub(s.received) as f64 / s.speed } else { f64::NAN };
            let tail = format!(
                " {} / {}  {}  ETA {}  {conns} conn",
                fmt::bytes(s.received),
                fmt::bytes(total),
                fmt::rate(s.speed),
                fmt::duration(eta)
            );
            let head = format!("{pct:5.1}%");
            let room = s.width.saturating_sub(head.chars().count() + tail.chars().count() + 3);
            if room >= 10 {
                let bar = segment_bar(s.segments, total, room.min(40));
                format!("{head} [{bar}]{tail}")
            } else {
                format!("{head}{tail}")
            }
        }
        _ => format!("{}  {}  {conns} conn", fmt::bytes(s.received), fmt::rate(s.speed)),
    }
}

/// One character per slice of the file showing how much of it has arrived:
/// `#` complete, `=` mostly, `-` started, `.` nothing yet.
pub fn segment_bar(segments: &[SegmentView], total: u64, width: usize) -> String {
    if width == 0 || total == 0 {
        return String::new();
    }
    (0..width)
        .map(|i| {
            let a = (i as u128 * total as u128 / width as u128) as u64;
            let b = ((i as u128 + 1) * total as u128 / width as u128) as u64;
            let cell = b.saturating_sub(a).max(1);
            let have: u64 = segments
                .iter()
                .map(|s| {
                    let lo = s.start.max(a);
                    let hi = (s.start + s.received).min(s.end).min(b);
                    hi.saturating_sub(lo)
                })
                .sum();
            let frac = have as f64 / cell as f64;
            if frac >= 0.999 {
                '#'
            } else if frac >= 0.5 {
                '='
            } else if frac > 0.0 {
                '-'
            } else {
                '.'
            }
        })
        .collect()
}

#[cfg(windows)]
pub fn terminal_width() -> Option<usize> {
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_ERROR_HANDLE,
    };
    // SAFETY: plain Win32 calls with a valid out-pointer to a zeroed struct.
    unsafe {
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(handle, &mut info) == 0 {
            return None;
        }
        let w = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
        (w > 0).then_some(w as usize)
    }
}

#[cfg(not(windows))]
pub fn terminal_width() -> Option<usize> {
    std::env::var("COLUMNS").ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: u64, end: u64, received: u64) -> SegmentView {
        SegmentView { start, end, received, active: true, speed: 0.0 }
    }

    #[test]
    fn bar_reflects_segments() {
        // Four quarters: complete, half, untouched, a sliver.
        let segs = [seg(0, 100, 100), seg(100, 200, 60), seg(200, 300, 0), seg(300, 400, 10)];
        assert_eq!(segment_bar(&segs, 400, 4), "#=.-");
        assert_eq!(segment_bar(&segs, 400, 8), "###-..-.");
        assert_eq!(segment_bar(&[], 400, 3), "...");
        assert_eq!(segment_bar(&segs, 0, 3), "");
    }

    #[test]
    fn progress_lines() {
        const MIB: u64 = 1 << 20;
        let segs = [seg(0, 5 * MIB, 5 * MIB), seg(5 * MIB, 10 * MIB, 0)];
        let mut s = Snapshot {
            phase: Phase::Downloading,
            total: Some(10 * MIB),
            received: 5 * MIB,
            speed: MIB as f64,
            connections: 8,
            segments: &segs,
            width: 79,
        };
        let line = render_line(&s);
        assert_eq!(line, " 50.0% [###########...........] 5.0 MiB / 10.0 MiB  1.0 MiB/s  ETA 0:05  8 conn");
        assert!(line.chars().count() <= 79);
        s.width = 200; // the bar is capped at 40 cells
        assert!(render_line(&s).contains(&format!("[{}{}]", "#".repeat(20), ".".repeat(20))));

        s.width = 0; // no terminal: no bar
        assert_eq!(render_line(&s), " 50.0% 5.0 MiB / 10.0 MiB  1.0 MiB/s  ETA 0:05  8 conn");
        s.speed = 0.0;
        assert!(render_line(&s).contains("ETA --:--"));
        s.total = None;
        assert_eq!(render_line(&s), "5.0 MiB  0 B/s  8 conn");
        s.phase = Phase::Verifying;
        assert_eq!(render_line(&s), "Verifying SHA-256...");
    }
}
