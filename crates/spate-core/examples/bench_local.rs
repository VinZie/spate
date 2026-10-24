//! Local throughput benchmark for the download engine.
//!
//! ```text
//! cargo run --release -p spate-core --example bench_local -- [size_mib] [conns,...] [dir]
//! ```
//!
//! Serves a pseudo-random body from an in-process HTTP/1.1 server on
//! loopback (its own runtime and threads) and downloads it with 1/4/8/16
//! connections. Two baselines put the numbers in context: reading the same
//! ranges with plain reqwest and discarding them (network + HTTP ceiling),
//! and writing the same amount with positional writes from N threads (disk
//! ceiling).

#[path = "../tests/common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{ServerConfig, TestServer, sha256_hex};
use spate_core::download::{self, DownloadRequest, EngineOptions, Outcome, Progress};
use spate_core::{fileio, http};
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let size_mib: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(512);
    let conns: Vec<usize> = args
        .get(1)
        .map(|s| s.split(',').filter_map(|c| c.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![1, 4, 8, 16]);
    let dir = args.get(2).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("spate-bench"));
    let size = size_mib * MIB;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    println!("generating {size_mib} MiB body...");
    let server = TestServer::start(ServerConfig { threads: 8, ..ServerConfig::sized(size) });
    let expected = server.sha256();
    let url = server.url("/bench/body.bin");
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("server on {}, {cpus} logical CPUs, output dir {}\n", server.addr, dir.display());

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    println!("{:<34} {:>6} {:>9} {:>10}", "case", "conns", "seconds", "MiB/s");
    for &n in &conns {
        let secs = rt.block_on(raw_http(&url, size, n));
        report("baseline: HTTP read, discard", n, size, secs);
    }
    for &n in &conns {
        let secs = raw_disk(&dir, size, n);
        report("baseline: positional file writes", n, size, secs);
    }
    if std::env::var_os("BENCH_DIAG").is_some() {
        for mode in [Mode::CopyOnly, Mode::Spin, Mode::Inline, Mode::Serial, Mode::Pipelined, Mode::Deep] {
            for &n in &conns {
                let secs = rt.block_on(read_then_write(&url, &dir, size, n, mode));
                report(&format!("diag: {mode:?}"), n, size, secs);
            }
        }
    }
    let reps: usize = std::env::var("BENCH_REPS").ok().and_then(|s| s.parse().ok()).unwrap_or(3).max(1);
    println!("(engine: best and worst of {reps} runs; 'transfer' excludes the final fsync + rename)");
    for &n in &conns {
        let mut runs = Vec::new();
        for _ in 0..reps {
            let (secs, transfer, path) = rt.block_on(engine(&url, &dir, n));
            let got = sha256_file(&path);
            assert_eq!(got, expected, "corrupt download with {n} connections");
            let _ = std::fs::remove_file(&path);
            runs.push((secs, transfer));
        }
        runs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (best, worst) = (runs[0], runs[runs.len() - 1]);
        report("engine: download::run (best)", n, size, best.0);
        report("  transfer phase (best run)", n, size, best.1);
        report("engine: download::run (worst)", n, size, worst.0);
    }
    println!("\nall downloads verified (sha256 {})", &expected[..16]);
    println!(
        "server sent {} MiB in {} requests over {} connections",
        server.stats().body_bytes() / MIB,
        server.stats().requests(),
        server.stats().connections.load(std::sync::atomic::Ordering::Relaxed)
    );
}

fn report(case: &str, conns: usize, size: u64, secs: f64) {
    println!("{case:<34} {conns:>6} {secs:>9.3} {:>10.0}", size as f64 / MIB as f64 / secs);
}

async fn raw_http(url: &str, size: u64, n: usize) -> f64 {
    let started = Instant::now();
    let part = size.div_ceil(n as u64);
    let mut tasks = Vec::new();
    for i in 0..n as u64 {
        let url = url.to_string();
        let (a, b) = (i * part, ((i + 1) * part).min(size));
        tasks.push(tokio::spawn(async move {
            let client = http::build_client(&http::NetConfig::default()).unwrap();
            let mut resp = client.get(url).header("range", format!("bytes={a}-{}", b - 1)).send().await.unwrap();
            let mut got = 0u64;
            while let Some(c) = resp.chunk().await.unwrap() {
                got += c.len() as u64;
            }
            assert_eq!(got, b - a);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    started.elapsed().as_secs_f64()
}

/// The engine's former I/O pattern without its bookkeeping: read into a
/// buffer, and every 512 KiB await a positional write on the blocking pool.
#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Copy into the buffer, never write.
    CopyOnly,
    /// Like CopyOnly, but stop reading for 500 us (busy) every 512 KiB.
    Spin,
    /// Write synchronously on the async worker thread.
    Inline,
    /// Await each write on the blocking pool before reading on.
    Serial,
    /// Keep reading while the previous write runs on the blocking pool.
    Pipelined,
    /// Up to 32 writes in flight, buffers recycled through a pool.
    Deep,
}

async fn deep_pipeline(url: String, file: Arc<std::fs::File>, a: u64, b: u64) {
    use std::collections::VecDeque;
    const CAP: usize = 1 << 20;
    let pool: Arc<parking_lot::Mutex<Vec<Vec<u8>>>> = Arc::default();
    let client = http::build_client(&http::NetConfig::default()).unwrap();
    let mut resp = client.get(url).header("range", format!("bytes={a}-{}", b - 1)).send().await.unwrap();
    let mut buf: Vec<u8> = Vec::with_capacity(CAP);
    let mut at = a;
    let mut q: VecDeque<tokio::task::JoinHandle<std::io::Result<()>>> = VecDeque::new();
    loop {
        let c = resp.chunk().await.unwrap();
        let end = c.is_none();
        if let Some(c) = &c {
            if buf.len() + c.len() > CAP && !buf.is_empty() {
                flush_deep(&mut buf, &mut at, &file, &pool, &mut q).await;
            }
            buf.extend_from_slice(c);
        }
        if (buf.len() >= CAP / 2 || end) && !buf.is_empty() {
            flush_deep(&mut buf, &mut at, &file, &pool, &mut q).await;
        }
        if end {
            break;
        }
    }
    for h in q {
        h.await.unwrap().unwrap();
    }
    assert_eq!(at, b);
}

async fn flush_deep(
    buf: &mut Vec<u8>,
    at: &mut u64,
    file: &Arc<std::fs::File>,
    pool: &Arc<parking_lot::Mutex<Vec<Vec<u8>>>>,
    q: &mut std::collections::VecDeque<tokio::task::JoinHandle<std::io::Result<()>>>,
) {
    while q.front().is_some_and(|h| h.is_finished()) || q.len() >= 32 {
        q.pop_front().unwrap().await.unwrap().unwrap();
    }
    let next = pool.lock().pop().unwrap_or_else(|| Vec::with_capacity(1 << 20));
    let data = std::mem::replace(buf, next);
    let (f, p, off) = (file.clone(), pool.clone(), *at);
    *at += data.len() as u64;
    q.push_back(tokio::task::spawn_blocking(move || {
        let r = fileio::write_all_at(&f, &data, off);
        let mut data = data;
        data.clear();
        p.lock().push(data);
        r
    }));
}

async fn read_then_write(url: &str, dir: &Path, size: u64, n: usize, mode: Mode) -> f64 {
    let path = dir.join(format!("diag-{n}.bin"));
    let file = Arc::new(fileio::create_preallocated(&path, Some(size)).unwrap());
    let started = Instant::now();
    let part = size.div_ceil(n as u64);
    let mut tasks = Vec::new();
    for i in 0..n as u64 {
        let (url, file) = (url.to_string(), file.clone());
        let (a, b) = (i * part, ((i + 1) * part).min(size));
        tasks.push(tokio::spawn(async move {
            if let Mode::Deep = mode {
                deep_pipeline(url, file, a, b).await;
                return 0;
            }
            let client = http::build_client(&http::NetConfig::default()).unwrap();
            let mut resp = client.get(url).header("range", format!("bytes={a}-{}", b - 1)).send().await.unwrap();
            let mut buf = bytes::BytesMut::with_capacity(576 * 1024);
            let mut at = a;
            let mut max_chunk = 0;
            let mut pending = None;
            loop {
                let c = resp.chunk().await.unwrap();
                if let Some(c) = &c {
                    max_chunk = max_chunk.max(c.len());
                    buf.extend_from_slice(c);
                }
                if buf.len() >= 512 * 1024 || (c.is_none() && !buf.is_empty()) {
                    let data = buf.split().freeze();
                    let f = file.clone();
                    let off = at;
                    at += data.len() as u64;
                    match mode {
                        Mode::CopyOnly => drop(data),
                        Mode::Spin => {
                            let t = Instant::now();
                            while t.elapsed() < Duration::from_micros(500) {
                                std::hint::spin_loop();
                            }
                        }
                        Mode::Inline => fileio::write_all_at(&f, &data, off).unwrap(),
                        Mode::Serial => tokio::task::spawn_blocking(move || fileio::write_all_at(&f, &data, off))
                            .await
                            .unwrap()
                            .unwrap(),
                        Mode::Deep => unreachable!(),
                        Mode::Pipelined => {
                            if let Some(p) = pending.take() {
                                let p: tokio::task::JoinHandle<std::io::Result<()>> = p;
                                p.await.unwrap().unwrap();
                            }
                            pending = Some(tokio::task::spawn_blocking(move || fileio::write_all_at(&f, &data, off)));
                        }
                    }
                }
                if c.is_none() {
                    break;
                }
            }
            if let Some(p) = pending.take() {
                p.await.unwrap().unwrap();
            }
            assert_eq!(at, b);
            max_chunk
        }));
    }
    let mut max_chunk = 0;
    for t in tasks {
        max_chunk = max_chunk.max(t.await.unwrap());
    }
    let secs = started.elapsed().as_secs_f64();
    println!("  (largest chunk {} KiB)", max_chunk / 1024);
    drop(file);
    let _ = std::fs::remove_file(&path);
    secs
}

fn raw_disk(dir: &Path, size: u64, n: usize) -> f64 {
    let path = dir.join(format!("raw-{n}.bin"));
    let block = vec![0x5Au8; 512 * 1024];
    let started = Instant::now();
    let file = Arc::new(fileio::create_preallocated(&path, Some(size)).unwrap());
    let part = size.div_ceil(n as u64);
    std::thread::scope(|s| {
        for i in 0..n as u64 {
            let (file, block) = (file.clone(), &block);
            s.spawn(move || {
                let (mut at, end) = (i * part, ((i + 1) * part).min(size));
                while at < end {
                    let len = (end - at).min(block.len() as u64) as usize;
                    fileio::write_all_at(&file, &block[..len], at).unwrap();
                    at += len as u64;
                }
            });
        }
    });
    file.sync_all().unwrap();
    let secs = started.elapsed().as_secs_f64();
    drop(file);
    let _ = std::fs::remove_file(&path);
    secs
}

/// Returns (total seconds, seconds until the Finishing phase, path).
async fn engine(url: &str, dir: &Path, n: usize) -> (f64, f64, PathBuf) {
    let req = DownloadRequest { url: url.to_string(), headers: Vec::new(), file_name: None, dir: dir.to_path_buf() };
    let opts = EngineOptions { connections: n, checkpoint_interval: Duration::from_secs(5), ..EngineOptions::default() };
    let progress = Arc::new(Progress::default());
    let started = Instant::now();
    let watch = {
        let progress = progress.clone();
        tokio::spawn(async move {
            while progress.phase() as u8 <= download::Phase::Downloading as u8 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            started.elapsed().as_secs_f64()
        })
    };
    let out = download::run(req, opts, None, progress, CancellationToken::new()).await.expect("download failed");
    let secs = started.elapsed().as_secs_f64();
    let transfer = watch.await.unwrap();
    match out {
        Outcome::Completed { path, .. } => (secs, transfer, path),
        Outcome::Paused => panic!("unexpected pause"),
    }
}

fn sha256_file(path: &Path) -> String {
    let data = std::fs::read(path).unwrap();
    sha256_hex(&data)
}
