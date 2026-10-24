//! A small, deterministic HTTP/1.1 file server for engine integration tests
//! and benchmarks. It runs on its own Tokio runtime (in a dedicated thread) so
//! it never competes with the engine under test for executor threads.
//!
//! Features: byte ranges with `If-Range`, optional `Accept-Ranges`, chunked
//! bodies without `Content-Length`, per-connection throttling, a cap on
//! concurrently served requests (extra requests get `503`), connection drops
//! and stalls injected into the first N body responses, runtime content/ETag
//! changes, rejecting every request from some point on, `Content-Disposition`,
//! a redirect path (also to another server) and keep-alive. The request log
//! keeps each request's headers.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// Inject a fault into the first `first_requests` body responses, after
/// `after_bytes` bytes of body have been sent.
#[derive(Clone, Copy, Debug)]
pub struct Fault {
    pub after_bytes: u64,
    pub first_requests: usize,
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub size: u64,
    pub seed: u64,
    /// Honour `Range` with `206 Partial Content`; otherwise always `200`.
    pub ranges: bool,
    /// Advertise `Accept-Ranges: bytes`.
    pub accept_ranges: bool,
    /// Per-connection throughput cap in bytes/s (changeable at runtime).
    pub throttle: Option<u64>,
    /// `(rate, n)`: the first `n` body responses are sent at `rate` instead.
    pub throttle_first: Option<(u64, usize)>,
    /// Requests served concurrently beyond this get `503` (changeable).
    pub max_concurrent: Option<usize>,
    /// Close the connection mid-body.
    pub drop_after: Option<Fault>,
    /// Stop sending mid-body and keep the connection open.
    pub stall_after: Option<Fault>,
    /// Wait this long before sending the response head of the first
    /// `.1` file requests.
    pub head_delay: Option<(Duration, usize)>,
    /// Serve bytes generated from this seed (same size and validators) to
    /// every second body request, like two mirrors that disagree.
    pub alternate_seed: Option<u64>,
    /// Answer the first `reject_first` file requests with `reject_status`.
    pub reject_first: usize,
    pub reject_status: u16,
    /// `Transfer-Encoding: chunked` and no `Content-Length` (implies no ranges).
    pub chunked: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_disposition: Option<String>,
    pub content_type: Option<String>,
    /// Requests for this path get `302 Found` pointing at `redirect_to` (a
    /// path, or a URL on another server).
    pub redirect_from: Option<String>,
    pub redirect_to: String,
    pub keep_alive: bool,
    pub threads: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            size: 4 << 20,
            seed: 1,
            ranges: true,
            accept_ranges: true,
            throttle: None,
            throttle_first: None,
            max_concurrent: None,
            drop_after: None,
            stall_after: None,
            head_delay: None,
            alternate_seed: None,
            reject_first: 0,
            reject_status: 503,
            chunked: false,
            etag: Some("\"v1\"".into()),
            last_modified: Some("Wed, 21 Oct 2026 07:28:00 GMT".into()),
            content_disposition: None,
            content_type: Some("application/octet-stream".into()),
            redirect_from: None,
            redirect_to: "/dl/actual.bin".into(),
            keep_alive: true,
            threads: 2,
        }
    }
}

impl ServerConfig {
    pub fn sized(size: u64) -> Self {
        Self { size, ..Self::default() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestLog {
    pub method: String,
    pub path: String,
    pub status: u16,
    /// Requested `[start, end)` when a `Range` header was sent.
    pub range: Option<(u64, u64)>,
    pub if_range: Option<String>,
    /// Content version that served the request.
    pub version: u64,
    /// Request headers, names in lower case.
    pub headers: Vec<(String, String)>,
}

impl RequestLog {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
pub struct Stats {
    /// Every request, including redirects, 503s and HEADs.
    pub requests: AtomicUsize,
    /// GET requests answered with a body (200/206).
    pub body_requests: AtomicUsize,
    /// File requests that got past the redirect / connection-limit checks.
    pub file_requests: AtomicUsize,
    forced_rejects: AtomicUsize,
    pub rejected_503: AtomicUsize,
    pub connections: AtomicUsize,
    active_conns: AtomicUsize,
    pub peak_conns: AtomicUsize,
    active_reqs: AtomicUsize,
    pub peak_reqs: AtomicUsize,
    /// Body bytes handed to the socket (includes bytes the client discarded).
    pub body_bytes: AtomicU64,
    log: Mutex<Vec<RequestLog>>,
}

impl Stats {
    pub fn log(&self) -> Vec<RequestLog> {
        self.log.lock().clone()
    }

    pub fn log_len(&self) -> usize {
        self.log.lock().len()
    }

    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    pub fn body_requests(&self) -> usize {
        self.body_requests.load(Ordering::SeqCst)
    }

    pub fn rejected(&self) -> usize {
        self.rejected_503.load(Ordering::SeqCst)
    }

    pub fn peak_conns(&self) -> usize {
        self.peak_conns.load(Ordering::SeqCst)
    }

    pub fn peak_reqs(&self) -> usize {
        self.peak_reqs.load(Ordering::SeqCst)
    }

    pub fn body_bytes(&self) -> u64 {
        self.body_bytes.load(Ordering::SeqCst)
    }
}

struct Content {
    body: Bytes,
    etag: Option<String>,
    last_modified: Option<String>,
    version: u64,
}

struct State {
    cfg: ServerConfig,
    alternate: Option<Bytes>,
    content: RwLock<Content>,
    throttle: AtomicU64,
    max_concurrent: AtomicUsize,
    /// `(n, status)`: request number `n` (counting from 0) and all later
    /// ones are answered with `status`.
    reject_from: Mutex<Option<(usize, u16)>>,
    stats: Stats,
    cancel: CancellationToken,
}

pub struct TestServer {
    pub addr: SocketAddr,
    state: Arc<State>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    pub fn start(cfg: ServerConfig) -> Self {
        let body = make_body(cfg.size, cfg.seed);
        let threads = cfg.threads.max(1);
        let alternate = cfg.alternate_seed.map(|seed| make_body(cfg.size, seed));
        let state = Arc::new(State {
            alternate,
            content: RwLock::new(Content {
                body,
                etag: cfg.etag.clone(),
                last_modified: cfg.last_modified.clone(),
                version: 0,
            }),
            throttle: AtomicU64::new(cfg.throttle.unwrap_or(0)),
            max_concurrent: AtomicUsize::new(cfg.max_concurrent.unwrap_or(0)),
            reject_from: Mutex::new(None),
            cfg,
            stats: Stats::default(),
            cancel: CancellationToken::new(),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let st = state.clone();
        let thread = std::thread::Builder::new()
            .name("test-http-server".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(threads)
                    .enable_all()
                    .build()
                    .expect("server runtime");
                rt.block_on(async move {
                    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
                    tx.send(listener.local_addr().unwrap()).unwrap();
                    loop {
                        tokio::select! {
                            _ = st.cancel.cancelled() => break,
                            r = listener.accept() => if let Ok((sock, _)) = r {
                                let st = st.clone();
                                tokio::spawn(async move { handle_connection(st, sock).await });
                            }
                        }
                    }
                });
                rt.shutdown_timeout(Duration::from_millis(200));
            })
            .expect("spawn server thread");
        let addr = rx.recv().expect("server address");
        Self { addr, state, thread: Some(thread) }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn stats(&self) -> &Stats {
        &self.state.stats
    }

    pub fn body(&self) -> Bytes {
        self.state.content.read().body.clone()
    }

    /// The other mirror's bytes (see [`ServerConfig::alternate_seed`]).
    pub fn alternate_body(&self) -> Option<Bytes> {
        self.state.alternate.clone()
    }

    pub fn sha256(&self) -> String {
        sha256_hex(&self.body())
    }

    pub fn version(&self) -> u64 {
        self.state.content.read().version
    }

    /// Replace the served file (new bytes and validators), as if it had been
    /// re-uploaded.
    pub fn change_content(&self, size: u64, seed: u64, etag: Option<&str>, last_modified: Option<&str>) {
        let body = make_body(size, seed);
        let mut c = self.state.content.write();
        c.body = body;
        c.etag = etag.map(str::to_string);
        c.last_modified = last_modified.map(str::to_string);
        c.version += 1;
    }

    pub fn set_throttle(&self, bytes_per_sec: Option<u64>) {
        self.state.throttle.store(bytes_per_sec.unwrap_or(0), Ordering::SeqCst);
    }

    pub fn set_max_concurrent(&self, max: Option<usize>) {
        self.state.max_concurrent.store(max.unwrap_or(0), Ordering::SeqCst);
    }

    /// Answer request number `n` (counting every request from 0, see
    /// [`Stats::requests`]) and all later ones with `status`.
    pub fn reject_from(&self, n: usize, status: u16) {
        *self.state.reject_from.lock() = Some((n, status));
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.state.cancel.cancel();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Deterministic pseudo-random bytes (splitmix64 of the word index), so a
/// block written at the wrong offset never looks right.
pub fn make_body(size: u64, seed: u64) -> Bytes {
    let mut v = vec![0u8; size as usize];
    let key = seed.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    for (i, chunk) in v.chunks_mut(8).enumerate() {
        let w = splitmix64(key ^ i as u64);
        chunk.copy_from_slice(&w.to_le_bytes()[..chunk.len()]);
    }
    Bytes::from(v)
}

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(data))
}

/// Assert that the file at `path` holds exactly `expected`, reporting the
/// first differing offset, and that the SHA-256 digests match.
pub fn assert_file_eq(path: &Path, expected: &[u8]) {
    let got = std::fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    assert_eq!(got.len(), expected.len(), "size of {}", path.display());
    if let Some(i) = got.iter().zip(expected).position(|(a, b)| a != b) {
        panic!("{} differs from the source at byte {i} (of {})", path.display(), expected.len());
    }
    assert_eq!(sha256_hex(&got), sha256_hex(expected));
}

/// A fresh, empty directory that is removed again on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let dir = std::env::temp_dir().join(format!("spate-it-{name}-{}-{n}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Names of the files currently in the directory, sorted.
    pub fn files(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.0)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

struct Guard<'a>(&'a AtomicUsize);

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn enter<'a>(active: &'a AtomicUsize, peak: &AtomicUsize) -> (usize, Guard<'a>) {
    let n = active.fetch_add(1, Ordering::SeqCst) + 1;
    peak.fetch_max(n, Ordering::SeqCst);
    (n, Guard(active))
}

async fn handle_connection(st: Arc<State>, sock: TcpStream) {
    let _ = sock.set_nodelay(true);
    st.stats.connections.fetch_add(1, Ordering::SeqCst);
    let (_, _conn_guard) = enter(&st.stats.active_conns, &st.stats.peak_conns);
    let mut io = BufReader::with_capacity(16 * 1024, sock);
    loop {
        let req = tokio::select! {
            _ = st.cancel.cancelled() => return,
            r = read_request(&mut io) => match r {
                Ok(Some(r)) => r,
                _ => return,
            },
        };
        let keep = match serve(&st, &mut io, &req).await {
            Ok(keep) => keep,
            Err(_) => return,
        };
        let client_close = req.header("connection").is_some_and(|v| v.eq_ignore_ascii_case("close"));
        if !keep || !st.cfg.keep_alive || client_close {
            let _ = io.get_mut().shutdown().await;
            return;
        }
    }
}

async fn read_request(io: &mut BufReader<TcpStream>) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    if io.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut headers = Vec::new();
    let mut total = 0;
    loop {
        let mut h = String::new();
        let n = io.read_line(&mut h).await?;
        total += n;
        if n == 0 || total > 64 * 1024 {
            return Ok(None);
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    // Requests from the engine never carry bodies, but drain one if present.
    if let Some(len) = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse::<u64>().ok()) {
        let mut sink = vec![0u8; len.min(1 << 20) as usize];
        io.read_exact(&mut sink).await?;
    }
    Ok(Some(Request { method, path, headers }))
}

/// Parse a single `bytes=` range against `size` into `[start, end)`.
/// `Err(())` means unsatisfiable.
fn parse_range(v: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = v.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multi-range: serve the whole file
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    Some(if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 || size == 0 { Err(()) } else { Ok((size.saturating_sub(n), size)) }
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() { size } else { (b.parse::<u64>().ok()? + 1).min(size) };
        if start >= size || start >= end { Err(()) } else { Ok((start, end)) }
    })
}

/// Serve one request. Returns whether the connection may be reused.
async fn serve(st: &State, io: &mut BufReader<TcpStream>, req: &Request) -> std::io::Result<bool> {
    let stats = &st.stats;
    let nth = stats.requests.fetch_add(1, Ordering::SeqCst);
    let cfg = &st.cfg;
    let (body, etag, last_modified, version) = {
        let c = st.content.read();
        (c.body.clone(), c.etag.clone(), c.last_modified.clone(), c.version)
    };
    let size = body.len() as u64;
    let range_hdr = req.header("range").map(str::to_string);
    let if_range = req.header("if-range").map(str::to_string);
    let log = |status: u16, range: Option<(u64, u64)>| {
        stats.log.lock().push(RequestLog {
            method: req.method.clone(),
            path: req.path.clone(),
            status,
            range,
            if_range: if_range.clone(),
            version,
            headers: req.headers.clone(),
        });
    };

    let reject = *st.reject_from.lock();
    if let Some((from, code)) = reject
        && nth >= from
    {
        log(code, None);
        let head = format!("HTTP/1.1 {code} Rejected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        io.get_mut().write_all(head.as_bytes()).await?;
        return Ok(false);
    }

    if cfg.redirect_from.as_deref() == Some(req.path.as_str()) {
        log(302, None);
        let head = format!(
            "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\n\r\n",
            cfg.redirect_to
        );
        io.get_mut().write_all(head.as_bytes()).await?;
        return Ok(true);
    }

    if stats.forced_rejects.fetch_add(1, Ordering::SeqCst) < cfg.reject_first {
        let code = cfg.reject_status;
        if code == 503 {
            stats.rejected_503.fetch_add(1, Ordering::SeqCst);
        }
        log(code, None);
        let head = format!("HTTP/1.1 {code} Rejected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        io.get_mut().write_all(head.as_bytes()).await?;
        return Ok(false);
    }

    let n = stats.active_reqs.fetch_add(1, Ordering::SeqCst) + 1;
    let _req_guard = Guard(&stats.active_reqs);
    let max = st.max_concurrent.load(Ordering::SeqCst);
    if max == 0 || n <= max {
        stats.peak_reqs.fetch_max(n, Ordering::SeqCst);
    } else {
        stats.rejected_503.fetch_add(1, Ordering::SeqCst);
        log(503, None);
        let head = "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        io.get_mut().write_all(head.as_bytes()).await?;
        return Ok(false);
    }

    let nth = stats.file_requests.fetch_add(1, Ordering::SeqCst);
    if let Some((delay, first)) = cfg.head_delay
        && nth < first
    {
        let mut probe = [0u8; 1];
        tokio::select! {
            _ = st.cancel.cancelled() => return Ok(false),
            _ = tokio::time::sleep(delay) => {}
            _ = io.read(&mut probe) => return Ok(false),
        }
    }

    let ranges = cfg.ranges && !cfg.chunked;
    let validator_ok = match if_range.as_deref() {
        None => true,
        Some(v) if v.starts_with('"') => etag.as_deref() == Some(v),
        Some(v) => last_modified.as_deref() == Some(v),
    };
    let range = if ranges && validator_ok { range_hdr.as_deref().and_then(|r| parse_range(r, size)) } else { None };

    let mut head = String::new();
    let (status, start, end) = match range {
        Some(Err(())) => {
            log(416, None);
            let h = format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\nContent-Length: 0\r\n\r\n");
            io.get_mut().write_all(h.as_bytes()).await?;
            return Ok(true);
        }
        Some(Ok((a, b))) => {
            head.push_str("HTTP/1.1 206 Partial Content\r\n");
            head.push_str(&format!("Content-Range: bytes {a}-{}/{size}\r\n", b - 1));
            (206, a, b)
        }
        None => {
            head.push_str("HTTP/1.1 200 OK\r\n");
            (200, 0, size)
        }
    };
    log(status, range_hdr.as_deref().and_then(|r| parse_range(r, size)).and_then(|r| r.ok()));
    if cfg.chunked {
        head.push_str("Transfer-Encoding: chunked\r\n");
    } else {
        head.push_str(&format!("Content-Length: {}\r\n", end - start));
    }
    if cfg.accept_ranges && !cfg.chunked {
        head.push_str("Accept-Ranges: bytes\r\n");
    }
    if let Some(e) = &etag {
        head.push_str(&format!("ETag: {e}\r\n"));
    }
    if let Some(lm) = &last_modified {
        head.push_str(&format!("Last-Modified: {lm}\r\n"));
    }
    if let Some(cd) = &cfg.content_disposition {
        head.push_str(&format!("Content-Disposition: {cd}\r\n"));
    }
    if let Some(ct) = &cfg.content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    head.push_str(if cfg.keep_alive { "Connection: keep-alive\r\n\r\n" } else { "Connection: close\r\n\r\n" });
    io.get_mut().write_all(head.as_bytes()).await?;
    if req.method.eq_ignore_ascii_case("HEAD") {
        return Ok(true);
    }

    let idx = stats.body_requests.fetch_add(1, Ordering::SeqCst);
    let source = match &st.alternate {
        Some(alt) if idx % 2 == 1 => alt.clone(),
        _ => body,
    };
    let data = source.slice(start as usize..end as usize);
    let rate = cfg.throttle_first.filter(|&(_, n)| idx < n).map(|(r, _)| r);
    let fault_hits = |f: Option<Fault>| f.filter(|f| idx < f.first_requests).map(|f| f.after_bytes);
    if let Some(k) = fault_hits(cfg.drop_after) {
        let k = (k as usize).min(data.len());
        send_body(st, io.get_mut(), &data[..k], false, rate).await?;
        // Abort the connection with the body incomplete.
        return Ok(false);
    }
    if let Some(k) = fault_hits(cfg.stall_after) {
        let k = (k as usize).min(data.len());
        send_body(st, io.get_mut(), &data[..k], false, rate).await?;
        // Go silent until the client gives up (closes) or the server stops.
        let mut probe = [0u8; 1];
        tokio::select! {
            _ = st.cancel.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_secs(120)) => {}
            _ = io.read(&mut probe) => {}
        }
        return Ok(false);
    }
    send_body(st, io.get_mut(), &data, cfg.chunked, rate).await?;
    Ok(true)
}

async fn send_body(
    st: &State,
    w: &mut TcpStream,
    data: &[u8],
    chunked: bool,
    rate_override: Option<u64>,
) -> std::io::Result<()> {
    let started = Instant::now();
    let mut sent = 0u64;
    let mut at = 0usize;
    while at < data.len() {
        let rate = rate_override.unwrap_or_else(|| st.throttle.load(Ordering::Relaxed));
        let piece = if rate > 0 { (rate / 64).clamp(1024, 64 * 1024) as usize } else { 256 * 1024 };
        let p = &data[at..(at + piece).min(data.len())];
        if rate > 0 {
            // Absolute schedule: coarse timer granularity only adds burstiness.
            let due = started + Duration::from_secs_f64(sent as f64 / rate as f64);
            tokio::time::sleep_until(due.into()).await;
        }
        if chunked {
            w.write_all(format!("{:x}\r\n", p.len()).as_bytes()).await?;
        }
        w.write_all(p).await?;
        if chunked {
            w.write_all(b"\r\n").await?;
        }
        at += p.len();
        sent += p.len() as u64;
        st.stats.body_bytes.fetch_add(p.len() as u64, Ordering::Relaxed);
    }
    if chunked {
        w.write_all(b"0\r\n\r\n").await?;
    }
    Ok(())
}
