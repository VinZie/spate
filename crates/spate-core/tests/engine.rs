//! End-to-end tests of the download engine against a local HTTP server.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{Fault, ServerConfig, TempDir, TestServer, assert_file_eq};
use spate_core::download::{self, DownloadRequest, EngineOptions, Outcome, Progress, ResumeData};
use spate_core::http::NetConfig;
use spate_core::{Error, RateLimiter};
use tokio_util::sync::CancellationToken;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

fn opts(connections: usize) -> EngineOptions {
    EngineOptions {
        connections,
        min_split: 256 * KIB,
        max_retries: 6,
        net: NetConfig { connect_timeout_secs: 5, read_timeout_secs: 5, ..NetConfig::default() },
        limiters: Vec::new(),
        checkpoint_interval: Duration::from_millis(100),
        expected_sha256: None,
    }
}

fn request(url: String, dir: &Path) -> DownloadRequest {
    DownloadRequest { url, headers: Vec::new(), file_name: None, dir: dir.to_path_buf() }
}

/// Peak of `Progress::active_connections()` sampled while a download runs.
struct ConnSampler {
    peak: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl ConnSampler {
    fn start(progress: &Arc<Progress>) -> Self {
        let peak = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s, prog) = (peak.clone(), stop.clone(), progress.clone());
        tokio::spawn(async move {
            while !s.load(Ordering::Relaxed) {
                p.fetch_max(prog.active_connections(), Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        Self { peak, stop }
    }

    fn finish(self) -> usize {
        self.stop.store(true, Ordering::Relaxed);
        self.peak.load(Ordering::Relaxed)
    }
}

struct Run {
    result: spate_core::Result<Outcome>,
    progress: Arc<Progress>,
    elapsed: Duration,
    peak_active: usize,
}

async fn run_download(req: DownloadRequest, opts: EngineOptions, resume: Option<ResumeData>) -> Run {
    let progress = Arc::new(Progress::default());
    let sampler = ConnSampler::start(&progress);
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        download::run(req, opts, resume, progress.clone(), CancellationToken::new()),
    )
    .await
    .expect("download hung");
    Run { result, elapsed: started.elapsed(), peak_active: sampler.finish(), progress }
}

fn completed(run: &Run) -> (PathBuf, u64) {
    match &run.result {
        Ok(Outcome::Completed { path, size }) => (path.clone(), *size),
        other => panic!("expected a completed download, got {other:?}"),
    }
}

async fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn no_part_files(dir: &TempDir) {
    let parts: Vec<String> = dir.files().into_iter().filter(|f| f.ends_with(download::PART_SUFFIX)).collect();
    assert!(parts.is_empty(), "leftover partial files: {parts:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_connection_is_much_faster_under_per_connection_throttle() {
    let size = 8 * MIB;
    let server = TestServer::start(ServerConfig { throttle: Some(2 * MIB), ..ServerConfig::sized(size) });
    let body = server.body();
    let dir = TempDir::new("speedup");

    let multi = run_download(request(server.url("/files/multi.bin"), dir.path()), opts(8), None).await;
    let (path, n) = completed(&multi);
    assert_eq!(n, size);
    assert_file_eq(&path, &body);
    assert!(multi.peak_active >= 6, "only {} connections were active at once", multi.peak_active);

    let single = run_download(request(server.url("/files/single.bin"), dir.path()), opts(1), None).await;
    let (path, _) = completed(&single);
    assert_file_eq(&path, &body);
    assert_eq!(single.peak_active, 1);

    let speedup = single.elapsed.as_secs_f64() / multi.elapsed.as_secs_f64();
    println!(
        "8 MiB @ 2 MiB/s per connection: 1 conn {:.2}s, 8 conns {:.2}s => {speedup:.1}x",
        single.elapsed.as_secs_f64(),
        multi.elapsed.as_secs_f64()
    );
    assert!(speedup >= 3.0, "speedup only {speedup:.2}x");
    no_part_files(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_without_ranges_uses_one_connection() {
    let size = 3 * MIB + 17;
    let server =
        TestServer::start(ServerConfig { ranges: false, accept_ranges: false, ..ServerConfig::sized(size) });
    let dir = TempDir::new("norange");
    let run = run_download(request(server.url("/files/plain.bin"), dir.path()), opts(8), None).await;
    let (path, n) = completed(&run);
    assert_eq!(n, size);
    assert_file_eq(&path, &server.body());
    let log = server.stats().log();
    assert_eq!(server.stats().body_requests(), 1, "a non-resumable file must be fetched exactly once: {log:?}");
    // (A sampled peak can read 0 when the whole transfer fits between samples.)
    assert!(run.peak_active <= 1);
    let info = run.progress.info().unwrap();
    assert!(!info.resumable);
    assert_eq!(info.total, Some(size));
    no_part_files(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ranges_without_accept_ranges_header_still_go_parallel() {
    // The probe asks for `bytes=0-`; a 206 proves range support even when the
    // server never advertises it.
    let size = 4 * MIB;
    let server = TestServer::start(ServerConfig { accept_ranges: false, ..ServerConfig::sized(size) });
    let dir = TempDir::new("noaccept");
    let run = run_download(request(server.url("/files/x.bin"), dir.path()), opts(4), None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    assert!(run.progress.info().unwrap().resumable);
    assert!(server.stats().body_requests() >= 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mid_stream_drops_are_retried() {
    let size = 4 * MIB;
    let server = TestServer::start(ServerConfig {
        drop_after: Some(Fault { after_bytes: 300 * KIB, first_requests: 6 }),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("drops");
    let run = run_download(request(server.url("/files/flaky.bin"), dir.path()), opts(4), None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    assert!(server.stats().body_requests() > 6, "dropped connections must be re-requested");
    assert_eq!(run.progress.received(), size, "received must count every byte exactly once");
    println!("drops: {:.2}s, {} requests", run.elapsed.as_secs_f64(), server.stats().requests());
    assert!(run.elapsed < Duration::from_secs(10), "recovery took {:?}", run.elapsed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_resumable_restart_resets_progress() {
    let size = 2 * MIB;
    let server = TestServer::start(ServerConfig {
        ranges: false,
        accept_ranges: false,
        drop_after: Some(Fault { after_bytes: 700 * KIB, first_requests: 2 }),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("norange-drop");
    let run = run_download(request(server.url("/files/plain.bin"), dir.path()), opts(4), None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    assert_eq!(server.stats().body_requests(), 3);
    assert_eq!(run.progress.received(), size, "bytes from abandoned attempts must not be counted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_connections_are_restarted() {
    let size = 3 * MIB;
    let server = TestServer::start(ServerConfig {
        stall_after: Some(Fault { after_bytes: 200 * KIB, first_requests: 2 }),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("stall");
    let mut o = opts(4);
    o.net.read_timeout_secs = 1;
    let run = run_download(request(server.url("/files/stall.bin"), dir.path()), o, None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    assert!(server.stats().body_requests() > 2);
    println!("stall: {:.2}s", run.elapsed.as_secs_f64());
    assert!(run.elapsed < Duration::from_secs(10), "took {:?}", run.elapsed);
}

/// Start a throttled download, pause it once `pause_at` bytes have arrived
/// and return the checkpoint.
async fn start_and_pause(server: &TestServer, req: &DownloadRequest, o: &EngineOptions, pause_at: u64) -> ResumeData {
    let progress = Arc::new(Progress::default());
    let cancel = CancellationToken::new();
    let task = tokio::spawn(download::run(req.clone(), o.clone(), None, progress.clone(), cancel.clone()));
    wait_for("progress before pausing", Duration::from_secs(20), || progress.received() >= pause_at).await;
    cancel.cancel();
    let out = tokio::time::timeout(Duration::from_secs(10), task).await.expect("pause hung").unwrap();
    assert!(matches!(out, Ok(Outcome::Paused)), "expected Paused, got {out:?}");
    let cp = progress.checkpoint().expect("a paused download has a checkpoint");
    let total = server.body().len() as u64;
    assert_eq!(cp.total, Some(total));
    assert!(cp.resumable);
    let written = cp.written();
    assert!(written > 0 && written < total, "written {written} of {total}");
    assert!(cp.part_path().exists());
    // Everything the checkpoint calls done must really be on disk.
    let part = std::fs::read(cp.part_path()).unwrap();
    let body = server.body();
    for s in &cp.segments {
        let (a, b) = (s.start as usize, (s.start + s.done) as usize);
        assert!(part[a..b] == body[a..b], "checkpointed region {a}..{b} is not on disk");
    }
    cp
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume_does_not_refetch_completed_bytes() {
    let size = 6 * MIB;
    let server = TestServer::start(ServerConfig { throttle: Some(MIB), ..ServerConfig::sized(size) });
    let dir = TempDir::new("pause");
    let req = request(server.url("/files/paused.bin"), dir.path());
    let o = opts(4);
    let cp = start_and_pause(&server, &req, &o, 3 * MIB / 2).await;

    let mark = server.stats().log_len();
    server.set_throttle(None);
    let run = run_download(req, o, Some(cp.clone())).await;
    let (path, _) = completed(&run);
    assert_eq!(path, cp.file_path, "a resumed download keeps its name");
    assert_file_eq(&path, &server.body());
    assert_eq!(run.progress.received(), size);
    no_part_files(&dir);

    // Requests may reach a little into finished data (the engine compares
    // those bytes across connections) but never start inside it.
    let done: Vec<(u64, u64)> = cp.segments.iter().filter(|s| s.done > 0).map(|s| (s.start, s.start + s.done)).collect();
    let mut requested = 0;
    let mut refetched = 0;
    let resumed_log = &server.stats().log()[mark..];
    for entry in resumed_log {
        let (a, b) = entry.range.unwrap_or_else(|| panic!("resume request without a range: {entry:?}"));
        requested += b - a;
        for &(da, db) in &done {
            assert!(a < da || a >= db, "resume request {a}..{b} starts inside finished {da}..{db}");
            let overlap = b.min(db).saturating_sub(a.max(da));
            assert!(overlap <= 16 * KIB, "resume re-requested {overlap} finished bytes ({a}..{b} vs {da}..{db})");
            refetched += overlap;
        }
    }
    assert!(requested >= size - cp.written());
    assert!(refetched <= 16 * KIB * resumed_log.len() as u64);
    println!(
        "pause/resume: {} of {size} bytes done before the pause; resume requested {requested} bytes ({refetched} overlap)",
        cp.written()
    );
}

async fn remote_change_while_paused(etag: Option<&str>, new_etag: Option<&str>) {
    let size = 4 * MIB;
    let server = TestServer::start(ServerConfig {
        throttle: Some(MIB),
        etag: etag.map(str::to_string),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("changed");
    let req = request(server.url("/files/changing.bin"), dir.path());
    let o = opts(4);
    let cp = start_and_pause(&server, &req, &o, MIB).await;

    // Same size, different bytes and validators.
    server.change_content(size, 777, new_etag, Some("Thu, 22 Oct 2026 08:00:00 GMT"));
    server.set_throttle(None);
    let run = run_download(req, o, Some(cp.clone())).await;
    let (path, _) = completed(&run);
    assert_eq!(path, cp.file_path);
    assert_file_eq(&path, &server.body());
    assert_eq!(run.progress.received(), size);
    no_part_files(&dir);
    assert_eq!(dir.files().len(), 1, "unexpected files: {:?}", dir.files());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn etag_change_while_paused_restarts_from_scratch() {
    remote_change_while_paused(Some("\"v1\""), Some("\"v2\"")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn last_modified_change_while_paused_restarts_from_scratch() {
    remote_change_while_paused(None, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_length_chunked_body() {
    let size = 3 * MIB + 5;
    let server = TestServer::start(ServerConfig { chunked: true, ranges: false, ..ServerConfig::sized(size) });
    let dir = TempDir::new("chunked");
    let run = run_download(request(server.url("/files/stream.bin"), dir.path()), opts(8), None).await;
    let (path, n) = completed(&run);
    assert_eq!(n, size);
    assert_file_eq(&path, &server.body());
    let info = run.progress.info().unwrap();
    assert_eq!(info.total, None);
    assert!(!info.resumable);
    assert_eq!(server.stats().body_requests(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_length_stream_restarts_after_drop() {
    let size = 2 * MIB;
    let server = TestServer::start(ServerConfig {
        chunked: true,
        ranges: false,
        drop_after: Some(Fault { after_bytes: 500 * KIB, first_requests: 1 }),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("chunked-drop");
    let run = run_download(request(server.url("/files/stream.bin"), dir.path()), opts(4), None).await;
    let (path, n) = completed(&run);
    assert_eq!(n, size);
    assert_file_eq(&path, &server.body());
    assert_eq!(run.progress.received(), size);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redirect_and_content_disposition_naming() {
    let server = TestServer::start(ServerConfig {
        redirect_from: Some("/go".into()),
        redirect_to: "/dl/actual.bin".into(),
        content_disposition: Some("attachment; filename=\"report final.pdf\"".into()),
        ..ServerConfig::sized(MIB)
    });
    let dir = TempDir::new("naming");
    let run = run_download(request(server.url("/go"), dir.path()), opts(4), None).await;
    let (path, _) = completed(&run);
    assert_eq!(path.file_name().unwrap(), "report final.pdf");
    assert_file_eq(&path, &server.body());
    let info = run.progress.info().unwrap();
    assert!(info.final_url.ends_with("/dl/actual.bin"), "final url {}", info.final_url);
    assert_eq!(info.file_name, "report final.pdf");
    assert!(server.stats().log().iter().any(|e| e.status == 302));

    // Without Content-Disposition the name comes from the redirect target.
    let server = TestServer::start(ServerConfig { redirect_from: Some("/go".into()), ..ServerConfig::sized(MIB) });
    let run = run_download(request(server.url("/go"), dir.path()), opts(4), None).await;
    let (path, _) = completed(&run);
    assert_eq!(path.file_name().unwrap(), "actual.bin");

    // An explicit name wins over everything.
    let mut req = request(server.url("/go"), dir.path());
    req.file_name = Some("chosen.dat".into());
    let run = run_download(req, opts(4), None).await;
    let (path, _) = completed(&run);
    assert_eq!(path.file_name().unwrap(), "chosen.dat");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_limited_server_still_completes() {
    let size = 6 * MIB;
    let server = TestServer::start(ServerConfig {
        throttle: Some(MIB),
        max_concurrent: Some(3),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("connlimit");
    let run = run_download(request(server.url("/files/limited.bin"), dir.path()), opts(8), None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    let stats = server.stats();
    println!(
        "conn limit: {:.2}s, peak active {}, server peak {}, 503s {}",
        run.elapsed.as_secs_f64(),
        run.peak_active,
        stats.peak_reqs(),
        stats.rejected()
    );
    assert!(stats.rejected() > 0, "the test should have provoked 503s");
    assert!(run.peak_active >= 3, "only {} connections were active", run.peak_active);
    assert!(stats.peak_reqs() >= 3);
    // 6 MiB at 3 x 1 MiB/s is 2 s; allow generous slack for retries.
    assert!(run.elapsed < Duration::from_secs(8), "took {:?}", run.elapsed);
}

/// Real-world pattern (e.g. Hetzner's speed-test hosts): two connections per
/// client, everything beyond is refused. Eight requested connections must
/// settle on two quickly, keep both busy and not hammer the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_client_cap_converges_without_hammering() {
    let size = 8 * MIB;
    let server = TestServer::start(ServerConfig {
        throttle: Some(MIB),
        max_concurrent: Some(2),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("cap2");
    let run = run_download(request(server.url("/files/cap2.bin"), dir.path()), opts(8), None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    let stats = server.stats();
    println!(
        "cap 2: {:.2}s, {} requests, {} refused, peak {}",
        run.elapsed.as_secs_f64(),
        stats.requests(),
        stats.rejected(),
        stats.peak_reqs()
    );
    assert_eq!(stats.peak_reqs(), 2, "both allowed connections should be used");
    // 8 MiB at 2 x 1 MiB/s is 4 s; one connection would need 8 s.
    assert!(run.elapsed < Duration::from_secs(6), "took {:?}", run.elapsed);
    // The initial burst is refused once; later probes for a third connection
    // are spaced out, so refusals stay bounded.
    assert!(stats.rejected() <= 10, "{} refused requests: hammering the server", stats.rejected());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sha256_verification() {
    let size = 2 * MIB;
    let server = TestServer::start(ServerConfig::sized(size));
    let dir = TempDir::new("sha");

    let mut o = opts(4);
    o.expected_sha256 = Some(server.sha256().to_uppercase());
    let run = run_download(request(server.url("/files/good.bin"), dir.path()), o, None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());

    let mut o = opts(4);
    o.expected_sha256 = Some("00".repeat(32));
    let run = run_download(request(server.url("/files/bad.bin"), dir.path()), o, None).await;
    match run.result {
        Err(Error::Checksum { expected, actual }) => {
            assert_eq!(expected, "00".repeat(32));
            assert_eq!(actual, server.sha256());
        }
        other => panic!("expected a checksum error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_byte_file() {
    for ranges in [true, false] {
        let server = TestServer::start(ServerConfig { ranges, accept_ranges: ranges, ..ServerConfig::sized(0) });
        let dir = TempDir::new("empty");
        let run = run_download(request(server.url("/files/empty.txt"), dir.path()), opts(8), None).await;
        let (path, n) = completed(&run);
        assert_eq!(n, 0);
        assert_eq!(path.file_name().unwrap(), "empty.txt");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        no_part_files(&dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limiter_caps_throughput() {
    let size = 5 * MIB / 2;
    let server = TestServer::start(ServerConfig::sized(size));
    let dir = TempDir::new("limit");
    let mut o = opts(4);
    o.limiters = vec![Arc::new(RateLimiter::new(MIB)), Arc::new(RateLimiter::new(0))];
    let run = run_download(request(server.url("/files/limited.bin"), dir.path()), o, None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    let secs = run.elapsed.as_secs_f64();
    let rate = size as f64 / secs / MIB as f64;
    println!("rate limit 1 MiB/s: {secs:.2}s => {rate:.2} MiB/s");
    assert!(rate < 1.25 && rate > 0.6, "observed {rate:.2} MiB/s");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn name_collisions_get_numbered() {
    let server = TestServer::start(ServerConfig::sized(256 * KIB));
    let dir = TempDir::new("collide");
    std::fs::write(dir.path().join("data.bin"), b"keep me").unwrap();
    let run = run_download(request(server.url("/files/data.bin"), dir.path()), opts(2), None).await;
    let (path, _) = completed(&run);
    assert_eq!(path.file_name().unwrap(), "data (2).bin");
    assert_file_eq(&path, &server.body());
    let run = run_download(request(server.url("/files/data.bin"), dir.path()), opts(2), None).await;
    let (path, _) = completed(&run);
    assert_eq!(path.file_name().unwrap(), "data (3).bin");
    assert_eq!(std::fs::read(dir.path().join("data.bin")).unwrap(), b"keep me");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_stealing_takes_over_from_a_slow_connection() {
    // The first connection crawls at 256 KiB/s while the others run at
    // 4 MiB/s. Without stealing, its quarter (2 MiB) alone would take 8 s;
    // the fast connections must split it up and finish it.
    let size = 8 * MIB;
    let server = TestServer::start(ServerConfig {
        throttle: Some(4 * MIB),
        throttle_first: Some((256 * KIB, 1)),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("steal");
    let mut o = opts(4);
    o.min_split = 64 * KIB;
    let run = run_download(request(server.url("/files/steal.bin"), dir.path()), o, None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    println!("steal: {:.2}s, {} requests", run.elapsed.as_secs_f64(), server.stats().requests());
    assert!(server.stats().requests() > 4, "nothing was stolen");
    assert!(run.elapsed < Duration::from_secs(4), "took {:?}", run.elapsed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inconsistent_mirrors_fall_back_to_one_connection() {
    // Every second response comes from a "mirror" with different bytes but
    // the same size and ETag, so only comparing data can notice.
    let size = 4 * MIB;
    let server = TestServer::start(ServerConfig {
        throttle: Some(4 * MIB),
        alternate_seed: Some(4242),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("mirrors");
    let run = run_download(request(server.url("/files/mirror.bin"), dir.path()), opts(4), None).await;
    let (path, _) = completed(&run);
    let got = std::fs::read(&path).unwrap();
    let (a, b) = (server.body(), server.alternate_body().unwrap());
    assert!(got[..] == a[..] || got[..] == b[..], "the result mixes bytes from both mirrors");
    assert!(server.stats().body_requests() > 4, "the mismatch must have caused a restart");
    let log = server.stats().log();
    let restart = log.iter().rposition(|e| e.range == Some((0, size))).expect("a restart from byte 0");
    assert!(
        log[restart + 1..].iter().all(|e| e.status != 206 || e.range.is_none_or(|(_, b)| b == size)),
        "after the mismatch the download must use a single connection"
    );
    no_part_files(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_connection_server_completes() {
    let size = 3 * MIB;
    let server = TestServer::start(ServerConfig {
        throttle: Some(2 * MIB),
        max_concurrent: Some(1),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("one-conn");
    let run = run_download(request(server.url("/files/one.bin"), dir.path()), opts(8), None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    println!("one-conn server: {:.2}s, {} 503s", run.elapsed.as_secs_f64(), server.stats().rejected());
    assert!(run.elapsed < Duration::from_secs(8), "took {:?}", run.elapsed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retired_connections_come_back_when_the_server_recovers() {
    // Only one connection is allowed at first, so the others retire. Once
    // the limit is lifted they must be re-added: one connection alone would
    // need 10 s.
    let size = 10 * MIB;
    let server = TestServer::start(ServerConfig {
        throttle: Some(MIB),
        max_concurrent: Some(1),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("recover");
    let req = request(server.url("/files/recover.bin"), dir.path());
    let task = tokio::spawn(run_download(req, opts(4), None));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    server.set_max_concurrent(None);
    let run = task.await.unwrap();
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    println!("recover: {:.2}s, peak active {}", run.elapsed.as_secs_f64(), run.peak_active);
    assert!(run.peak_active >= 3, "retired connections were not replaced (peak {})", run.peak_active);
    assert!(run.elapsed < Duration::from_secs(8), "took {:?}", run.elapsed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_with_missing_part_file_starts_over_under_the_same_name() {
    let size = 2 * MIB;
    let server = TestServer::start(ServerConfig { throttle: Some(MIB), ..ServerConfig::sized(size) });
    let dir = TempDir::new("lost-part");
    let req = request(server.url("/files/lost.bin"), dir.path());
    let o = opts(2);
    let cp = start_and_pause(&server, &req, &o, MIB / 2).await;
    std::fs::remove_file(cp.part_path()).unwrap();
    server.set_throttle(None);
    let run = run_download(req, o, Some(cp.clone())).await;
    let (path, _) = completed(&run);
    assert_eq!(path, cp.file_path);
    assert_file_eq(&path, &server.body());
    assert_eq!(run.progress.received(), size);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_rejects_a_corrupt_segment_map() {
    let size = 2 * MIB;
    let server = TestServer::start(ServerConfig { throttle: Some(MIB), ..ServerConfig::sized(size) });
    let dir = TempDir::new("bad-map");
    let req = request(server.url("/files/map.bin"), dir.path());
    let o = opts(2);
    let mut cp = start_and_pause(&server, &req, &o, MIB / 2).await;
    // Claim more than was written: trusting this would leave zeros behind.
    cp.segments[0].end += 1;
    server.set_throttle(None);
    let run = run_download(req, o, Some(cp.clone())).await;
    let (path, _) = completed(&run);
    assert_eq!(path, cp.file_path);
    assert_file_eq(&path, &server.body());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_connection_reuses_its_keep_alive_socket_across_segments() {
    let size = 4 * MIB;
    let server = TestServer::start(ServerConfig { throttle: Some(MIB), ..ServerConfig::sized(size) });
    let dir = TempDir::new("keepalive");
    let req = request(server.url("/files/ka.bin"), dir.path());
    let cp = start_and_pause(&server, &req, &opts(4), MIB).await;
    let unfinished = cp.segments.iter().filter(|s| !s.is_complete()).count();
    assert!(unfinished >= 2, "want several segments left, got {:?}", cp.segments);

    server.set_throttle(None);
    let (conns, reqs) = (server.stats().connections.load(Ordering::SeqCst), server.stats().requests());
    let run = run_download(req, opts(1), Some(cp)).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    let new_conns = server.stats().connections.load(Ordering::SeqCst) - conns;
    let new_reqs = server.stats().requests() - reqs;
    assert_eq!(new_reqs, unfinished);
    assert_eq!(new_conns, 1, "{new_reqs} requests should share one connection");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_survives_tiny_checkpoint_interval() {
    let server = TestServer::start(ServerConfig::sized(2 * MIB));
    let dir = TempDir::new("ckpt0");
    let mut o = opts(4);
    o.checkpoint_interval = Duration::ZERO;
    let run = run_download(request(server.url("/files/c.bin"), dir.path()), o, None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    assert!(run.progress.checkpoint().is_none(), "a finished download has no checkpoint");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_during_probe_is_a_clean_pause() {
    // The server sits on the first request without answering; cancelling
    // must still return promptly, as a pause.
    let server = TestServer::start(ServerConfig {
        head_delay: Some((Duration::from_secs(30), 1)),
        ..ServerConfig::sized(MIB)
    });
    let dir = TempDir::new("probe-cancel");
    let progress = Arc::new(Progress::default());
    let cancel = CancellationToken::new();
    let task =
        tokio::spawn(download::run(request(server.url("/files/p.bin"), dir.path()), opts(4), None, progress, cancel.clone()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    cancel.cancel();
    let out = tokio::time::timeout(Duration::from_secs(5), task).await.expect("cancel hung").unwrap();
    assert!(matches!(out, Ok(Outcome::Paused)), "got {out:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unanswered_requests_time_out_and_are_retried() {
    // The probe and the next two requests never get a response head.
    let size = 2 * MIB;
    let server = TestServer::start(ServerConfig {
        head_delay: Some((Duration::from_secs(60), 3)),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("slow-head");
    let mut o = opts(4);
    o.net.read_timeout_secs = 1;
    let run = run_download(request(server.url("/files/slow.bin"), dir.path()), o, None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    println!("slow head: {:.2}s", run.elapsed.as_secs_f64());
    assert!(run.elapsed < Duration::from_secs(15), "took {:?}", run.elapsed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_probe_failure_is_retried() {
    // The very first response is cut off after a few bytes.
    let size = MIB;
    let server = TestServer::start(ServerConfig {
        max_concurrent: Some(8),
        drop_after: Some(Fault { after_bytes: 10, first_requests: 1 }),
        ..ServerConfig::sized(size)
    });
    let dir = TempDir::new("probe-retry");
    let run = run_download(request(server.url("/files/p.bin"), dir.path()), opts(4), None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());

    // A 503 on the probe itself is retried too.
    let server = TestServer::start(ServerConfig { reject_first: 2, ..ServerConfig::sized(size) });
    let dir = TempDir::new("probe-503");
    let run = run_download(request(server.url("/files/p.bin"), dir.path()), opts(2), None).await;
    let (path, _) = completed(&run);
    assert_file_eq(&path, &server.body());
    assert_eq!(server.stats().rejected(), 2);

    // A permanent error is not retried.
    let server = TestServer::start(ServerConfig { reject_first: 1, reject_status: 404, ..ServerConfig::sized(size) });
    let dir = TempDir::new("probe-404");
    let run = run_download(request(server.url("/files/p.bin"), dir.path()), opts(2), None).await;
    assert!(matches!(run.result, Err(Error::Status(404))), "got {:?}", run.result);
    assert_eq!(server.stats().requests(), 1);
    assert!(dir.files().is_empty(), "nothing may be created for a failed probe: {:?}", dir.files());
}
