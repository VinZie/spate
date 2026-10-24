//! `bench`: download the same URL with different connection counts and
//! compare throughput. Outputs go to a temporary folder and are deleted.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Args;
use spate_core::download::{self, DownloadRequest, Outcome, Progress};
use spate_core::{Settings, paths};
use tokio_util::sync::CancellationToken;

use crate::progress::Display;
use crate::{EXIT_ERROR, EXIT_OK, EXIT_PAUSED, NetArgs, fmt, get, options};

#[derive(Args, Debug)]
pub struct BenchArgs {
    #[arg(value_name = "URL")]
    pub url: String,
    /// Connection counts to try, comma separated
    #[arg(
        short,
        long,
        value_name = "LIST",
        value_delimiter = ',',
        default_value = "1,4,8,16",
        value_parser = clap::value_parser!(u16).range(1..=64)
    )]
    pub connections: Vec<u16>,
    /// Stop each run after this many bytes (e.g. 50M) instead of the whole file
    #[arg(long, value_name = "SIZE", value_parser = options::parse_size)]
    pub max_bytes: Option<u64>,
    /// Smallest piece worth splitting off for another connection [default: 1M]
    #[arg(long, value_name = "SIZE", value_parser = options::parse_size)]
    pub min_split: Option<u64>,
    #[command(flatten)]
    pub net: NetArgs,
}

struct Row {
    connections: usize,
    bytes: u64,
    secs: f64,
    peak_connections: usize,
}

enum RunEnd {
    Row(Row),
    Interrupted,
    Failed(String),
}

pub async fn run(args: BenchArgs) -> u8 {
    let counts: Vec<usize> = args.connections.iter().map(|&n| usize::from(n)).collect();
    let settings = Settings::load(&paths::data_dir());
    let stop = CancellationToken::new();
    {
        let stop = stop.clone();
        tokio::spawn(async move {
            if get::interrupted().await.is_ok() {
                stop.cancel();
            }
        });
    }

    let limit = args.max_bytes.map(|b| format!(" (first {} of each run)", fmt::bytes(b))).unwrap_or_default();
    eprintln!("Benchmarking {}{limit}", args.url);
    let mut rows = Vec::new();
    for &n in &counts {
        match run_once(&args, &settings, n, &stop).await {
            RunEnd::Row(row) => {
                eprintln!(
                    "  {n:>2} connection(s): {} in {} = {:.2} MB/s",
                    fmt::bytes(row.bytes),
                    fmt::elapsed(row.secs),
                    mb_per_sec(&row)
                );
                rows.push(row);
            }
            RunEnd::Interrupted => {
                eprintln!("Interrupted.");
                print_table(&rows);
                return EXIT_PAUSED;
            }
            RunEnd::Failed(e) => {
                eprintln!("  {n:>2} connection(s): failed: {e}");
            }
        }
    }
    if rows.is_empty() {
        eprintln!("error: every run failed");
        return EXIT_ERROR;
    }
    print_table(&rows);
    EXIT_OK
}

fn mb_per_sec(row: &Row) -> f64 {
    row.bytes as f64 / row.secs.max(1e-6) / 1e6
}

fn print_table(rows: &[Row]) {
    if rows.is_empty() {
        return;
    }
    let base = mb_per_sec(&rows[0]);
    println!();
    println!("{:>11} {:>12} {:>9} {:>9} {:>9} {:>8} {:>7}", "connections", "bytes", "time (s)", "MB/s", "Mbit/s", "speedup", "peak");
    for r in rows {
        let mbs = mb_per_sec(r);
        println!(
            "{:>11} {:>12} {:>9.2} {:>9.2} {:>9.1} {:>7.2}x {:>7}",
            r.connections,
            r.bytes,
            r.secs,
            mbs,
            mbs * 8.0,
            if base > 0.0 { mbs / base } else { 0.0 },
            r.peak_connections
        );
    }
}

async fn run_once(args: &BenchArgs, settings: &Settings, connections: usize, stop: &CancellationToken) -> RunEnd {
    let dir = std::env::temp_dir().join(format!("spate-bench-{}-{connections}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let end = measure(args, settings, connections, stop, dir.clone()).await;
    let _ = std::fs::remove_dir_all(&dir);
    end
}

async fn measure(
    args: &BenchArgs,
    settings: &Settings,
    connections: usize,
    stop: &CancellationToken,
    dir: PathBuf,
) -> RunEnd {
    let req = DownloadRequest { url: args.url.clone(), headers: args.net.headers(), file_name: None, dir };
    let opts = get::engine_options(settings, &args.net, Some(connections as u16), args.min_split);
    let progress = Arc::new(Progress::default());
    let cancel = stop.child_token();
    let started = Instant::now();
    let mut task = tokio::spawn(download::run(req, opts, None, progress.clone(), cancel.clone()));
    let mut display = Display::new(false);
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut redraw = Instant::now();
    let mut capped: Option<(u64, f64)> = None;
    let mut peak = 0;
    let joined = loop {
        tokio::select! {
            r = &mut task => break r,
            _ = tick.tick() => {
                peak = peak.max(progress.active_connections());
                let got = progress.received();
                if capped.is_none() && args.max_bytes.is_some_and(|m| got >= m) {
                    capped = Some((got, started.elapsed().as_secs_f64()));
                    cancel.cancel();
                }
                if redraw.elapsed() >= Duration::from_millis(250) {
                    redraw = Instant::now();
                    display.update(&progress);
                }
            }
        }
    };
    display.clear();
    let secs = started.elapsed().as_secs_f64();
    if stop.is_cancelled() {
        return RunEnd::Interrupted;
    }
    match joined {
        Ok(Ok(Outcome::Completed { size, .. })) => {
            RunEnd::Row(Row { connections, bytes: size, secs, peak_connections: peak })
        }
        Ok(Ok(Outcome::Paused)) | Ok(Err(_)) if capped.is_some() => {
            let (bytes, secs) = capped.unwrap_or_default();
            RunEnd::Row(Row { connections, bytes, secs, peak_connections: peak })
        }
        Ok(Ok(Outcome::Paused)) => RunEnd::Failed("stopped".into()),
        Ok(Err(e)) => RunEnd::Failed(fmt::error_chain(&e)),
        Err(e) => RunEnd::Failed(e.to_string()),
    }
}
