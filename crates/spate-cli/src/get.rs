//! `get`: headless download with live progress, Ctrl+C pause and automatic
//! resume through a `<file>.part.json` sidecar next to the partial file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use clap::Args;
use spate_core::download::{self, DownloadRequest, EngineOptions, Outcome, Progress, ResumeData};
use spate_core::{RateLimiter, Settings, naming, paths};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::progress::Display;
use crate::{EXIT_ERROR, EXIT_OK, EXIT_PAUSED, NetArgs, fmt, options};

pub const SIDECAR_SUFFIX: &str = ".part.json";
const SIDECAR_VERSION: u32 = 1;

#[derive(Args, Debug)]
pub struct GetArgs {
    /// One or more URLs, downloaded one after another
    #[arg(required = true, value_name = "URL")]
    pub urls: Vec<String>,
    /// Folder to save into [default: current folder]
    #[arg(short, long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// File name to save as (single URL only)
    #[arg(short, long, value_name = "NAME")]
    pub output: Option<String>,
    /// Parallel connections [default: from Spate settings, normally 8]
    #[arg(short, long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..=64))]
    pub connections: Option<u16>,
    /// Speed limit in bytes per second: 500K, 2M, ...
    #[arg(long, value_name = "RATE", value_parser = options::parse_size)]
    pub limit: Option<u64>,
    /// Smallest piece worth splitting off for another connection [default: 1M]
    #[arg(long, value_name = "SIZE", value_parser = options::parse_size)]
    pub min_split: Option<u64>,
    /// Verify the finished file against this SHA-256 (single URL only)
    #[arg(long, value_name = "HEX", value_parser = options::parse_sha256)]
    pub sha256: Option<String>,
    /// Ignore an existing partial download and start from scratch
    #[arg(long)]
    pub no_resume: bool,
    /// No progress output (errors are still printed)
    #[arg(short, long)]
    pub quiet: bool,
    #[command(flatten)]
    pub net: NetArgs,
    /// Testing aid: behave as if Ctrl+C was pressed after this many seconds
    #[arg(long, hide = true, value_name = "SECS")]
    pub stop_after_secs: Option<f64>,
}

/// On-disk resume record: the engine's [`ResumeData`] plus the URL it is for.
#[derive(Debug, Serialize, Deserialize)]
pub struct Sidecar {
    pub spate_sidecar: u32,
    pub url: String,
    pub resume: ResumeData,
}

pub fn sidecar_path(file_path: &Path) -> PathBuf {
    naming::with_suffix(file_path, SIDECAR_SUFFIX)
}

#[derive(Debug)]
pub struct Found {
    pub path: PathBuf,
    pub resume: ResumeData,
}

/// Find the sidecar of an interrupted download of `url` in `dir`. With an
/// explicit output name only that file's sidecar is considered. Sidecars
/// whose partial file has disappeared are deleted.
pub fn find_sidecar(dir: &Path, url: &str, output: Option<&str>) -> Option<Found> {
    let candidates: Vec<PathBuf> = match output {
        Some(name) => vec![sidecar_path(&dir.join(naming::sanitize(name.trim())))],
        None => std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(SIDECAR_SUFFIX)))
            .collect(),
    };
    let url = url.trim();
    let mut best: Option<(SystemTime, Found)> = None;
    for path in candidates {
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(sidecar) = serde_json::from_slice::<Sidecar>(&bytes) else { continue };
        if sidecar.url.trim() != url && sidecar.resume.final_url.trim() != url {
            continue;
        }
        let mut resume = sidecar.resume;
        // The partial file lives next to its sidecar, even if the folder was
        // moved since: derive the destination from the sidecar's own path.
        let name = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(SIDECAR_SUFFIX));
        let (Some(name), Some(parent)) = (name, path.parent()) else { continue };
        resume.file_path = parent.join(name);
        if !resume.part_path().is_file() {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| modified > *t) {
            best = Some((modified, Found { path, resume }));
        }
    }
    best.map(|(_, f)| f)
}

pub fn write_sidecar(url: &str, resume: &ResumeData) -> std::io::Result<PathBuf> {
    let path = sidecar_path(&resume.file_path);
    let sidecar = Sidecar { spate_sidecar: SIDECAR_VERSION, url: url.to_string(), resume: resume.clone() };
    let json = serde_json::to_vec_pretty(&sidecar).map_err(std::io::Error::other)?;
    paths::write_atomic(&path, &json)?;
    Ok(path)
}

/// Keeps the sidecar in step with the engine's durable checkpoints so that
/// even a killed process can resume from the last few seconds.
struct SidecarKeeper {
    url: String,
    last_written: Option<u64>,
    last_at: Instant,
    paths: Vec<PathBuf>,
}

impl SidecarKeeper {
    fn new(url: &str, existing: Option<PathBuf>) -> Self {
        Self { url: url.into(), last_written: None, last_at: Instant::now(), paths: existing.into_iter().collect() }
    }

    fn save(&mut self, cp: &ResumeData) -> Option<PathBuf> {
        if !(cp.resumable && cp.total.is_some()) {
            return None;
        }
        match write_sidecar(&self.url, cp) {
            Ok(p) => {
                if !self.paths.contains(&p) {
                    self.paths.push(p.clone());
                }
                self.last_written = Some(cp.written());
                self.last_at = Instant::now();
                Some(p)
            }
            Err(e) => {
                eprintln!("\nwarning: cannot write resume data: {e}");
                None
            }
        }
    }

    fn tick(&mut self, p: &Progress) {
        if self.last_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        if let Some(cp) = p.checkpoint()
            && self.last_written != Some(cp.written())
        {
            self.save(&cp);
        }
    }

    /// Persist the final checkpoint, or remove stale sidecars if there is none.
    fn finish(&mut self, p: &Progress) -> Option<PathBuf> {
        match p.checkpoint() {
            Some(cp) if cp.resumable && cp.total.is_some() => self.save(&cp),
            _ => {
                self.remove();
                None
            }
        }
    }

    fn remove(&mut self) {
        for p in self.paths.drain(..) {
            let _ = std::fs::remove_file(p);
        }
    }
}

enum Finish {
    Done,
    Paused,
    Failed,
}

pub async fn run(args: GetArgs) -> u8 {
    if args.urls.len() > 1 && (args.output.is_some() || args.sha256.is_some()) {
        eprintln!("error: --output and --sha256 can only be used with a single URL");
        return EXIT_ERROR;
    }
    let settings = Settings::load(&paths::data_dir());
    let dir = match std::path::absolute(args.dir.clone().unwrap_or_else(|| PathBuf::from("."))) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: invalid folder: {e}");
            return EXIT_ERROR;
        }
    };

    let stop = CancellationToken::new();
    spawn_interrupt_handler(stop.clone(), args.quiet);
    if let Some(secs) = args.stop_after_secs {
        let s = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(secs.max(0.0))).await;
            s.cancel();
        });
    }
    // One limiter for the whole invocation.
    let limiter = args.limit.filter(|&l| l > 0).map(|l| Arc::new(RateLimiter::new(l)));

    let mut failed = 0;
    for url in &args.urls {
        match download_one(url, &args, &dir, &settings, limiter.clone(), &stop).await {
            Finish::Done => {}
            Finish::Paused => return EXIT_PAUSED,
            Finish::Failed => failed += 1,
        }
    }
    if failed > 0 { EXIT_ERROR } else { EXIT_OK }
}

fn spawn_interrupt_handler(stop: CancellationToken, quiet: bool) {
    tokio::spawn(async move {
        if interrupted().await.is_err() {
            return;
        }
        if !quiet {
            eprintln!("\nPausing... (press Ctrl+C again to quit immediately)");
        }
        stop.cancel();
        if interrupted().await.is_ok() {
            std::process::exit(i32::from(EXIT_PAUSED));
        }
    });
}

/// Ctrl+C (and on Windows also Ctrl+Break, which is what reaches a process
/// started in its own console process group).
pub async fn interrupted() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let mut brk = tokio::signal::windows::ctrl_break()?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r,
            _ = brk.recv() => Ok(()),
        }
    }
    #[cfg(not(windows))]
    {
        tokio::signal::ctrl_c().await
    }
}

pub fn engine_options(
    settings: &Settings,
    net: &NetArgs,
    connections: Option<u16>,
    min_split: Option<u64>,
) -> EngineOptions {
    EngineOptions {
        connections: connections.map(usize::from).unwrap_or(settings.connections),
        min_split: min_split.unwrap_or(settings.min_split_kib.saturating_mul(1024)).max(16 * 1024),
        max_retries: settings.max_retries,
        net: net.net_config(settings.net.clone()),
        checkpoint_interval: Duration::from_secs(2),
        ..EngineOptions::default()
    }
}

async fn download_one(
    url: &str,
    args: &GetArgs,
    dir: &Path,
    settings: &Settings,
    limiter: Option<Arc<RateLimiter>>,
    stop: &CancellationToken,
) -> Finish {
    let mut display = Display::new(args.quiet);
    let mut resume = None;
    let mut existing = None;
    if let Some(found) = find_sidecar(dir, url, args.output.as_deref()) {
        if args.no_resume {
            let _ = std::fs::remove_file(found.resume.part_path());
            let _ = std::fs::remove_file(&found.path);
        } else {
            if !args.quiet {
                let total = found.resume.total.map(fmt::bytes).unwrap_or_default();
                display.println(&format!(
                    "Resuming {} ({} of {total} already downloaded)",
                    found.resume.file_path.display(),
                    fmt::bytes(found.resume.written())
                ));
            }
            display.set_base(found.resume.written());
            existing = Some(found.path);
            resume = Some(found.resume);
        }
    }

    let req = DownloadRequest {
        url: url.to_string(),
        headers: args.net.headers(),
        file_name: args.output.clone(),
        dir: dir.to_path_buf(),
    };
    let mut opts = engine_options(settings, &args.net, args.connections, args.min_split);
    opts.limiters = limiter.into_iter().collect();
    opts.expected_sha256 = args.sha256.clone();

    let progress = Arc::new(Progress::default());
    let cancel = stop.child_token();
    let mut task = tokio::spawn(download::run(req, opts, resume, progress.clone(), cancel.clone()));
    let mut keeper = SidecarKeeper::new(url, existing);
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let joined = loop {
        tokio::select! {
            r = &mut task => break r,
            _ = tick.tick() => {
                display.update(&progress);
                keeper.tick(&progress);
            }
        }
    };
    display.clear();
    let result = joined.unwrap_or_else(|e| Err(spate_core::Error::Other(format!("download task failed: {e}"))));

    match result {
        Ok(Outcome::Completed { path, size }) => {
            keeper.remove();
            display.done(&path, size);
            Finish::Done
        }
        Ok(Outcome::Paused) => {
            report_pause(&mut display, &mut keeper, &progress, args.quiet);
            Finish::Paused
        }
        // Cancelled before the first response (e.g. Ctrl+C while connecting).
        Err(_) if cancel.is_cancelled() => {
            report_pause(&mut display, &mut keeper, &progress, args.quiet);
            Finish::Paused
        }
        Err(e) => {
            let kept = keeper.finish(&progress);
            eprintln!("error: {url}: {}", fmt::error_chain(&e));
            if kept.is_some() {
                eprintln!("The partial download was kept; run the same command again to resume it.");
            }
            Finish::Failed
        }
    }
}

fn report_pause(display: &mut Display, keeper: &mut SidecarKeeper, progress: &Progress, quiet: bool) {
    let saved = keeper.finish(progress);
    if quiet {
        return;
    }
    // Bytes safely on disk, which is what the next run continues from.
    let received = progress.checkpoint().map(|c| c.written()).unwrap_or_else(|| progress.received());
    match (saved, progress.info().and_then(|i| i.total)) {
        (Some(path), Some(total)) => display.println(&format!(
            "Paused at {:.1}% ({} of {}). Run the same command again to resume.\nResume data: {}",
            received as f64 * 100.0 / total.max(1) as f64,
            fmt::bytes(received),
            fmt::bytes(total),
            path.display()
        )),
        _ if progress.info().is_some_and(|i| !i.resumable) => {
            display.println("Stopped. The server does not support resuming, so the next run starts over.")
        }
        _ => display.println("Stopped."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spate_core::segments::Segment;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("spate-cli-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn resume_for(dir: &Path, name: &str) -> ResumeData {
        ResumeData {
            final_url: "https://cdn.example/f.bin".into(),
            file_path: dir.join(name),
            total: Some(100),
            resumable: true,
            etag: None,
            last_modified: None,
            segments: vec![Segment { start: 0, end: 100, done: 40 }],
        }
    }

    #[test]
    fn sidecar_roundtrip_and_lookup() {
        let dir = temp_dir("sidecar");
        let r = resume_for(&dir, "f.bin");
        std::fs::write(r.part_path(), [0u8; 100]).unwrap();
        let path = write_sidecar("https://example/f.bin", &r).unwrap();
        assert_eq!(path, dir.join("f.bin.part.json"));

        let found = find_sidecar(&dir, "https://example/f.bin", None).expect("found by URL");
        assert_eq!(found.resume, r);
        assert!(find_sidecar(&dir, "https://cdn.example/f.bin", None).is_some(), "final URL matches too");
        assert!(find_sidecar(&dir, "https://example/other.bin", None).is_none());
        assert!(find_sidecar(&dir, "https://example/f.bin", Some("f.bin")).is_some());
        assert!(find_sidecar(&dir, "https://example/f.bin", Some("g.bin")).is_none());

        // A sidecar without its partial file is stale and gets cleaned up.
        std::fs::remove_file(r.part_path()).unwrap();
        assert!(find_sidecar(&dir, "https://example/f.bin", None).is_none());
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn moved_folder_is_rebased() {
        let dir = temp_dir("moved");
        let mut r = resume_for(&dir, "f.bin");
        std::fs::write(r.part_path(), [0u8; 100]).unwrap();
        r.file_path = PathBuf::from("Z:\\gone\\f.bin");
        let json = serde_json::to_vec(&Sidecar { spate_sidecar: 1, url: "u".into(), resume: r }).unwrap();
        std::fs::write(dir.join("f.bin.part.json"), json).unwrap();
        let found = find_sidecar(&dir, "u", None).unwrap();
        assert_eq!(found.resume.file_path, dir.join("f.bin"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
