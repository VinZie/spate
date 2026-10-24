//! The multi-connection HTTP download engine.
//!
//! Flow: one probe request (`Range: bytes=0-`) discovers size, name and range
//! support, and its response body becomes the first connection's stream. The
//! other connections immediately split the file between them. Whenever a
//! connection finishes it steals half (speed-weighted) of the segment that is
//! projected to finish last, so the download ends with every connection busy.
//! All connections write directly into one preallocated file.
//!
//! Disk writes never stall a connection's socket: received bytes are batched
//! into pooled buffers and written on the blocking pool while the connection
//! keeps reading, bounded by a per-download in-flight budget. A batch is
//! committed to the segment table (and so becomes part of a checkpoint) only
//! once its write has completed, in order.
//!
//! Robustness: requests that get no response head or no data for
//! `read_timeout_secs` are restarted; failed connections retry with backoff,
//! and retire when others are healthy (servers that cap connections), with
//! retired slots refilled gradually. Range requests reach a little past the
//! segment end, and those bytes are compared with what the neighbouring
//! connection received for the same offsets; a mismatch (or a changed
//! validator) restarts the download from scratch, with a single connection
//! if the data itself disagreed.
//!
//! Safety: redirects are followed by [`http::get`] rather than by reqwest, and every hop
//! gets the headers scoped to it, so cookies and `Authorization` only ever
//! reach the origin they were collected for. A partial file ends with a
//! random token that the resume data records; a file without it belongs to
//! someone else and is never resumed into or deleted. Completed files are
//! verified before they get their final name, and carry the Mark of the Web.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use reqwest::StatusCode;
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::{Error, Result};
use crate::fileio;
use crate::http::{self, NetConfig, header_str};
use crate::limiter::RateLimiter;
use crate::naming;
use crate::segments::{Segment, SegmentTable, SegmentView, UNKNOWN_END};

pub const PART_SUFFIX: &str = ".part";
const MIB: u64 = 1024 * 1024;
/// A connection hands data to the disk in batches of at least this size.
const FLUSH_AT: usize = 512 * 1024;
/// ...or once its oldest byte has waited this long (slow connections), so
/// checkpoints stay fresh.
const FLUSH_AFTER: Duration = Duration::from_secs(1);
/// Capacity of the pooled batch buffers.
const BUF_CAP: usize = 1024 * 1024;
/// How many bytes a download's connections may run ahead of the disk.
const WRITE_BUDGET_KIB: u32 = 32 * 1024;
/// Range requests ask for this much past the segment end, so that every
/// boundary between two connections' data gets compared.
const OVERLAP: u64 = 16 * 1024;
/// Upper bound of bytes compared (and possibly kept) per boundary.
const CROSS_CHECK_MAX: usize = OVERLAP as usize;
/// Restarts from scratch (file changed on the server, data mismatch).
const MAX_RESTARTS: u32 = 2;
/// Spacing of attempts to replace retired connections (doubles on failure).
const REINFORCE_MIN: Duration = Duration::from_secs(2);
const REINFORCE_MAX: Duration = Duration::from_secs(60);
/// Length of the random token that marks a partial file as one download's.
const TOKEN_LEN: usize = 16;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    /// Extra request headers such as `Cookie`, `Referer` or `User-Agent`.
    pub headers: Vec<(String, String)>,
    /// Overrides the name suggested by the server.
    pub file_name: Option<String>,
    pub dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct EngineOptions {
    pub connections: usize,
    /// Segments smaller than twice this are never split further.
    pub min_split: u64,
    pub max_retries: u32,
    pub net: NetConfig,
    /// Every limiter is applied to every byte (e.g. global and per-download).
    pub limiters: Vec<Arc<RateLimiter>>,
    pub checkpoint_interval: Duration,
    /// Lower-case hex SHA-256 to verify after completion.
    pub expected_sha256: Option<String>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            connections: 8,
            min_split: MIB,
            max_retries: 8,
            net: NetConfig::default(),
            limiters: Vec::new(),
            checkpoint_interval: Duration::from_secs(5),
            expected_sha256: None,
        }
    }
}

/// Everything needed to continue an interrupted download.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResumeData {
    pub final_url: String,
    /// Destination path; data is written to this path plus [`PART_SUFFIX`].
    pub file_path: PathBuf,
    pub total: Option<u64>,
    pub resumable: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub segments: Vec<Segment>,
    /// Hex of the random token at the end of the partial file. Resume data
    /// without one (or whose file doesn't end with it) claims no file.
    #[serde(default)]
    pub part_token: Option<String>,
}

impl ResumeData {
    pub fn part_path(&self) -> PathBuf {
        naming::with_suffix(&self.file_path, PART_SUFFIX)
    }

    pub fn written(&self) -> u64 {
        self.segments.iter().map(|s| s.done).sum()
    }

    fn token(&self) -> Option<[u8; TOKEN_LEN]> {
        hex::decode(self.part_token.as_deref()?).ok()?.try_into().ok()
    }

    /// The same download with nothing written and no file claimed: the next
    /// run starts over under the same name.
    fn start_over(&self) -> Self {
        Self { segments: Vec::new(), part_token: None, ..self.clone() }
    }
}

/// Delete the partial file of `resume`, but only if it is still the one that
/// download wrote: another download may have created a file of the same name
/// since. Returns whether a file was deleted.
pub fn discard_part(resume: &ResumeData) -> bool {
    let path = resume.part_path();
    let ours = resume.token().is_some_and(|t| File::open(&path).is_ok_and(|f| has_token(&f, &t)));
    ours && std::fs::remove_file(&path).is_ok()
}

fn new_token() -> Result<[u8; TOKEN_LEN]> {
    let mut token = [0u8; TOKEN_LEN];
    getrandom::fill(&mut token).map_err(|e| Error::Other(format!("no random numbers: {e}")))?;
    Ok(token)
}

/// Whether `file` ends with `token`.
fn has_token(file: &File, token: &[u8; TOKEN_LEN]) -> bool {
    let Ok(len) = file.metadata().map(|m| m.len()) else { return false };
    let mut tail = [0u8; TOKEN_LEN];
    len >= TOKEN_LEN as u64 && fileio::read_exact_at(file, &mut tail, len - TOKEN_LEN as u64).is_ok() && tail == *token
}

/// Cut `file` to `at` bytes and append `token`.
fn write_token(file: &File, at: u64, token: &[u8; TOKEN_LEN]) -> io::Result<()> {
    file.set_len(at)?;
    fileio::write_all_at(file, token, at)
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileInfo {
    pub file_name: String,
    pub file_path: PathBuf,
    pub total: Option<u64>,
    pub resumable: bool,
    pub content_type: Option<String>,
    pub final_url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Connecting = 0,
    Downloading = 1,
    Finishing = 2,
    Verifying = 3,
}

/// Live state shared between the engine and whoever displays it.
#[derive(Default)]
pub struct Progress {
    received: AtomicU64,
    active: AtomicUsize,
    phase: AtomicU8,
    table: Mutex<SegmentTable>,
    info: Mutex<Option<FileInfo>>,
    checkpoint: Mutex<Option<ResumeData>>,
}

impl Progress {
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    pub fn active_connections(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Relaxed) {
            1 => Phase::Downloading,
            2 => Phase::Finishing,
            3 => Phase::Verifying,
            _ => Phase::Connecting,
        }
    }

    pub fn info(&self) -> Option<FileInfo> {
        self.info.lock().clone()
    }

    pub fn segments(&self) -> Vec<SegmentView> {
        self.table.lock().view()
    }

    /// Latest durable resume point (written data has been flushed to disk).
    pub fn checkpoint(&self) -> Option<ResumeData> {
        self.checkpoint.lock().clone()
    }

    pub(crate) fn set_phase(&self, p: Phase) {
        self.phase.store(p as u8, Ordering::Relaxed);
    }

    pub(crate) fn set_info(&self, info: FileInfo) {
        *self.info.lock() = Some(info);
    }

    pub(crate) fn add_received(&self, n: u64) {
        self.received.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn set_received(&self, n: u64) {
        self.received.store(n, Ordering::Relaxed);
    }

    /// Lets non-HTTP engines (HLS) report how many fetches are in flight.
    pub(crate) fn active_counter(&self) -> &AtomicUsize {
        &self.active
    }
}

#[derive(Debug)]
pub enum Outcome {
    Completed { path: PathBuf, size: u64 },
    /// Stopped by the caller; resume from [`Progress::checkpoint`] (which is
    /// `None` if the download was stopped before the server answered).
    Paused,
}

/// Download `req`. Cancelling `cancel` pauses the download cleanly; the
/// resume point is then available from `progress.checkpoint()`.
pub async fn run(
    req: DownloadRequest,
    opts: EngineOptions,
    resume: Option<ResumeData>,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> Result<Outcome> {
    let mut req = req;
    let mut opts = opts;
    let mut resume = resume;
    let mut restarts = 0;
    loop {
        let corrupted = Arc::new(AtomicBool::new(false));
        match run_once(&req, &opts, resume.take(), &progress, &cancel, &corrupted).await {
            // The server copy changed (or connections disagreed about its
            // bytes): start over under the same name.
            Err(Error::RemoteChanged) if restarts < MAX_RESTARTS && !cancel.is_cancelled() => {
                restarts += 1;
                let checkpoint = progress.checkpoint.lock().take();
                if let Some(cp) = checkpoint {
                    discard_part(&cp);
                    keep_name(&mut req, &cp.file_path);
                    // Should the fresh attempt fail before it has a file of
                    // its own, no resume point may still name the old one.
                    *progress.checkpoint.lock() = Some(cp.start_over());
                }
                if corrupted.load(Ordering::Relaxed) || restarts > 1 {
                    opts.connections = 1;
                }
                tracing::info!(restarts, connections = opts.connections, "remote file changed, restarting");
                progress.set_received(0);
            }
            other => return other,
        }
    }
}

/// Make a fresh attempt reuse the name and folder of an earlier one.
fn keep_name(req: &mut DownloadRequest, path: &Path) {
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
        req.dir = dir.to_path_buf();
        req.file_name = Some(name.to_string_lossy().into_owned());
    }
}

/// Delay before retry number `failures` (1-based) of a connection that made
/// no progress.
/// Map an unexpected status; 429/503 mean "too many connections/requests".
fn status_error(resp: &reqwest::Response, status: StatusCode) -> Error {
    match status {
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
            Error::Throttled { status: status.as_u16(), retry_after: http::retry_after(resp.headers()) }
        }
        s => Error::Status(s.as_u16()),
    }
}

fn backoff(failures: u32) -> Duration {
    Duration::from_millis(250u64 << failures.saturating_sub(1).min(7)).min(Duration::from_secs(30))
}

fn head_timeout(net: &NetConfig) -> Duration {
    Duration::from_secs(net.read_timeout_secs.max(1))
}

async fn run_once(
    req: &DownloadRequest,
    opts: &EngineOptions,
    resume: Option<ResumeData>,
    progress: &Arc<Progress>,
    cancel: &CancellationToken,
    corrupted: &Arc<AtomicBool>,
) -> Result<Outcome> {
    progress.set_phase(Phase::Connecting);
    let url = Url::parse(req.url.trim()).map_err(|e| Error::InvalidUrl(e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::InvalidUrl(format!("unsupported scheme `{}`", url.scheme())));
    }
    // Cookies and `Authorization` were collected for this URL, also when
    // resuming or restarting.
    let origin = url.clone();
    let headers = http::header_map(&req.headers);
    let first_client = http::build_client(&opts.net)?;

    let mut fresh = None;
    let resumed = match resume.map(|r| Setup::resumed(url.clone(), r, opts)) {
        Some(Ok(setup)) => Some(setup),
        Some(Err(stale)) => {
            // The partial data cannot be trusted (or used): start over. Its
            // file goes only if it is still ours; if another download has
            // taken the name since, the fresh attempt gets another one.
            tracing::info!(path = %stale.part_path().display(), "resume data unusable, restarting");
            discard_part(&stale);
            *progress.checkpoint.lock() = Some(stale.start_over());
            let mut r = req.clone();
            keep_name(&mut r, &stale.file_path);
            fresh = Some(r);
            None
        }
        None => None,
    };
    let probe_req = fresh.as_ref().unwrap_or(req);
    let (setup, first_response) = match resumed {
        Some(setup) => (setup, None),
        None => match Setup::probe(url, probe_req, &headers, &first_client, opts, cancel).await? {
            Some((setup, resp)) => (setup, Some(resp)),
            None => return Ok(Outcome::Paused),
        },
    };

    *progress.table.lock() = setup.table;
    progress.set_received(progress.table.lock().written());
    progress.set_info(setup.info.clone());

    let file = Arc::new(setup.file);
    let ctx = Arc::new(Ctx {
        url: setup.request_url,
        origin,
        headers,
        token: setup.token,
        writer: Writer {
            file: file.clone(),
            budget: Arc::new(Semaphore::new(WRITE_BUDGET_KIB as usize)),
            pool: Arc::new(Mutex::new(Vec::new())),
        },
        sync_handle: fileio::open_partial(&setup.part_path).ok().map(Arc::new),
        progress: progress.clone(),
        stop: cancel.child_token(),
        user_cancel: cancel.clone(),
        opts: opts.clone(),
        info: setup.info,
        etag: setup.etag,
        last_modified: setup.last_modified,
        corrupted: corrupted.clone(),
        overlaps: Mutex::new(Vec::new()),
        fatal: Mutex::new(None),
        last_error: Mutex::new(None),
        limit: AtomicUsize::new(usize::MAX),
        alive: AtomicUsize::new(0),
        opening: AtomicUsize::new(0),
        throttle: Mutex::new(Throttle::default()),
    });
    drop(file);
    let target = ctx.worker_count();
    let learned = learned_limit(&ctx.host());
    ctx.limit.store(learned.map_or(target, |n| n.clamp(1, target)), Ordering::SeqCst);
    if learned.is_some() {
        // Treat the remembered cap like a fresh throttle so we probe upward
        // only after a quiet period.
        ctx.throttle.lock().last = Some(Instant::now());
    }
    ctx.publish_checkpoint(ctx.progress.table.lock().snapshot());

    let complete_now = progress.table.lock().is_complete();
    if !complete_now {
        progress.set_phase(Phase::Downloading);
        let start_with = ctx.limit.load(Ordering::SeqCst);
        let mut set = JoinSet::new();
        // Claim only when there is a probe body to stream (`zip` would claim
        // eagerly and strand the slot on resume).
        #[allow(clippy::manual_option_zip)]
        let initial = first_response.and_then(|r| ctx.progress.table.lock().claim(0).map(|slot| (slot, r)));
        set.spawn(worker(ctx.clone(), 0, first_client, initial, None));
        for id in 1..start_with {
            set.spawn(worker(ctx.clone(), id, http::build_client(&opts.net)?, None, None));
        }
        let mut next_id = start_with;
        let mut probe_up_after = PROBE_UP_AFTER;
        let mut last_increase: Option<Instant> = None;
        let checkpoint_stop = CancellationToken::new();
        let checkpointer = tokio::spawn(checkpoint_loop(ctx.clone(), checkpoint_stop.clone()));

        // Connections that retired (typically because the server limits
        // connections per client) are replaced one at a time while work
        // remains. A replacement that gets data speeds up the next one; one
        // that fails doubles the wait.
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut delay = REINFORCE_MIN;
        let mut next_at = Instant::now() + delay;
        let mut scout: Option<Arc<AtomicBool>> = None;
        loop {
            tokio::select! {
                joined = set.join_next() => if joined.is_none() { break },
                _ = tick.tick() => {
                    let now = Instant::now();
                    if scout.as_ref().is_some_and(|s| s.load(Ordering::Relaxed)) {
                        scout = None;
                        delay = REINFORCE_MIN;
                        next_at = next_at.min(now + delay);
                    }
                    let (last_throttle, held_off) = {
                        let t = ctx.throttle.lock();
                        (t.last, t.not_before.is_some_and(|nb| now < nb))
                    };
                    // Additive increase: while the server stays quiet, allow
                    // one more connection per interval; each increase that
                    // gets refused doubles the interval.
                    if let Some(inc) = last_increase
                        && last_throttle.is_some_and(|t| t > inc)
                    {
                        probe_up_after = (probe_up_after * 2).min(Duration::from_secs(300));
                        last_increase = None;
                    }
                    let limit = ctx.limit.load(Ordering::SeqCst);
                    let quiet = last_throttle.is_some_and(|t| now.duration_since(t) >= probe_up_after);
                    let spaced = last_increase.is_none_or(|i| now.duration_since(i) >= probe_up_after);
                    if limit < target && quiet && spaced {
                        ctx.limit.store(limit + 1, Ordering::SeqCst);
                        last_increase = Some(now);
                    }
                    let wanted = now >= next_at
                        && !held_off
                        && !ctx.stop.is_cancelled()
                        && set.len() < ctx.limit.load(Ordering::SeqCst).min(target)
                        && ctx.progress.table.lock().can_claim();
                    if wanted && let Ok(client) = http::build_client(&opts.net) {
                        let flag = Arc::new(AtomicBool::new(false));
                        set.spawn(worker(ctx.clone(), next_id, client, None, Some(flag.clone())));
                        next_id += 1;
                        scout = Some(flag);
                        next_at = now + delay;
                        delay = (delay * 2).min(REINFORCE_MAX);
                    }
                }
            }
        }
        // Let an in-flight checkpoint finish so no file handle outlives us.
        checkpoint_stop.cancel();
        let _ = checkpointer.await;

        let limit = ctx.limit.load(Ordering::SeqCst);
        let mut caps = HOST_LIMITS.lock();
        if limit < target {
            caps.insert(ctx.host(), (limit, Instant::now()));
        } else {
            caps.remove(&ctx.host());
        }
    }

    let fatal = ctx.fatal.lock().take();
    if let Some(err) = fatal {
        ctx.final_checkpoint().await;
        return Err(err);
    }
    if !progress.table.lock().is_complete() {
        ctx.final_checkpoint().await;
        if ctx.user_cancel.is_cancelled() {
            return Ok(Outcome::Paused);
        }
        let err = ctx.last_error.lock().take();
        return Err(err.unwrap_or(Error::Other("all connections failed".into())));
    }

    finish(ctx).await
}

struct Setup {
    request_url: Url,
    file: File,
    part_path: PathBuf,
    token: [u8; TOKEN_LEN],
    table: SegmentTable,
    info: FileInfo,
    etag: Option<String>,
    last_modified: Option<String>,
}

/// Whether `segments` cover `[0, total)` exactly, with sane progress.
fn segments_tile(segments: &[Segment], total: u64) -> bool {
    let mut segs = segments.to_vec();
    segs.sort_by_key(|s| s.start);
    let mut at = 0;
    for s in &segs {
        if s.start != at || s.end < s.start || s.done > s.end - s.start {
            return false;
        }
        at = s.end;
    }
    at == total
}

impl Setup {
    /// Reopen a partial download, or hand the resume data back if it cannot
    /// be used (no range support, missing, resized or foreign part file, bad
    /// map).
    fn resumed(url: Url, r: ResumeData, opts: &EngineOptions) -> std::result::Result<Self, ResumeData> {
        let Some(total) = r.total.filter(|_| r.resumable) else { return Err(r) };
        if !segments_tile(&r.segments, total) {
            return Err(r);
        }
        let Some(token) = r.token() else { return Err(r) };
        let Ok(file) = fileio::open_partial(&r.part_path()) else { return Err(r) };
        // A file of this name that doesn't end with our token was created by
        // another download after this resume point was saved.
        if !file.metadata().is_ok_and(|m| m.len() == total + TOKEN_LEN as u64) || !has_token(&file, &token) {
            return Err(r);
        }
        let file_name = r.file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Self {
            // Re-resolve redirects: signed CDN links in `final_url` expire.
            request_url: url,
            part_path: r.part_path(),
            file,
            token,
            table: SegmentTable::from_segments(r.segments, opts.min_split),
            info: FileInfo {
                file_name,
                file_path: r.file_path,
                total: r.total,
                resumable: true,
                content_type: None,
                final_url: r.final_url,
            },
            etag: r.etag,
            last_modified: r.last_modified,
        })
    }

    /// First request, retried on transient failures. `None` = cancelled.
    async fn probe(
        url: Url,
        req: &DownloadRequest,
        headers: &HeaderMap,
        client: &reqwest::Client,
        opts: &EngineOptions,
        cancel: &CancellationToken,
    ) -> Result<Option<(Self, reqwest::Response)>> {
        let mut failures = 0;
        loop {
            let attempt = tokio::select! {
                r = Self::probe_once(url.clone(), req, headers, client, opts) => r,
                _ = cancel.cancelled() => return Ok(None),
            };
            match attempt {
                Ok(found) => return Ok(Some(found)),
                Err(e) if e.is_transient() && failures < opts.max_retries => {
                    failures += 1;
                    tracing::debug!(error = %e, failures, "probe failed, retrying");
                    let wait = match e {
                        Error::Throttled { retry_after: Some(ra), .. } => ra.min(MAX_RETRY_AFTER).max(backoff(failures)),
                        _ => backoff(failures),
                    };
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = cancel.cancelled() => return Ok(None),
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn probe_once(
        url: Url,
        req: &DownloadRequest,
        headers: &HeaderMap,
        client: &reqwest::Client,
        opts: &EngineOptions,
    ) -> Result<(Self, reqwest::Response)> {
        let timeout = head_timeout(&opts.net);
        let send = |range: bool| {
            let mut extra = HeaderMap::new();
            if range {
                extra.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
            }
            tokio::time::timeout(timeout, http::get(client, &url, &url, headers, extra, opts.net.max_redirects, |_| Ok(())))
        };
        let stalled = |_| Error::Stalled(timeout.as_secs());
        let mut resp = send(true).await.map_err(stalled)??;
        if resp.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            // `bytes=0-` cannot be satisfied for an empty file; ask plainly.
            resp = send(false).await.map_err(stalled)??;
        }
        let status = resp.status();
        let h = resp.headers();
        let (total, resumable) = match status {
            StatusCode::PARTIAL_CONTENT => {
                let (first, _, total) = header_str(h, header::CONTENT_RANGE)
                    .and_then(http::parse_content_range)
                    .ok_or_else(|| Error::Other("server sent an invalid Content-Range".into()))?;
                if first != 0 {
                    return Err(Error::Other("server ignored the requested range".into()));
                }
                (total, total.is_some())
            }
            StatusCode::OK => {
                let total = resp.content_length();
                let ranges = header_str(h, header::ACCEPT_RANGES).is_some_and(|v| v.eq_ignore_ascii_case("bytes"));
                (total, ranges && total.is_some())
            }
            s => return Err(status_error(&resp, s)),
        };

        let final_url = resp.url().clone();
        let content_type = header_str(h, header::CONTENT_TYPE).map(str::to_string);
        let etag = header_str(h, header::ETAG).filter(|e| !e.starts_with("W/")).map(str::to_string);
        let last_modified = header_str(h, header::LAST_MODIFIED).map(str::to_string);

        let mut name = req
            .file_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(naming::sanitize)
            .or_else(|| header_str(h, header::CONTENT_DISPOSITION).and_then(naming::from_content_disposition))
            .or_else(|| naming::from_url(&final_url))
            .or_else(|| naming::from_url(&url))
            .unwrap_or_else(|| "download".to_string());
        if naming::split_ext(&name).1.is_empty()
            && let Some(ext) = content_type.as_deref().and_then(naming::extension_for_mime)
        {
            name = format!("{name}.{ext}");
        }

        std::fs::create_dir_all(&req.dir)?;
        let token = new_token()?;
        let (file_path, part_path, file) = create_part(&req.dir, &name)?;
        if let Err(e) = prepare_part(&file, &req.dir, total, resumable, &token) {
            drop(file);
            let _ = std::fs::remove_file(&part_path);
            return Err(e);
        }
        let file_name = file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name);

        let min_split = if resumable { opts.min_split } else { u64::MAX / 4 };
        let table = SegmentTable::new(total.unwrap_or(UNKNOWN_END), min_split);
        Ok((
            Self {
                request_url: final_url.clone(),
                file,
                part_path,
                token,
                table,
                info: FileInfo {
                    file_name,
                    file_path,
                    total,
                    resumable,
                    content_type,
                    final_url: final_url.to_string(),
                },
                etag,
                last_modified,
            },
            resp,
        ))
    }
}

/// Create the partial file for a download named `name` in `dir`, numbering
/// the name as needed. `create_new`: two downloads that pick the same name at
/// the same moment never share a file. Returns the destination, the partial
/// file's path and the file.
fn create_part(dir: &Path, name: &str) -> Result<(PathBuf, PathBuf, File)> {
    let mut attempts = 0;
    loop {
        let file_path = naming::unique_path(dir, name, PART_SUFFIX);
        let part_path = naming::with_suffix(&file_path, PART_SUFFIX);
        match OpenOptions::new().read(true).write(true).create_new(true).open(&part_path) {
            Ok(file) => return Ok((file_path, part_path, file)),
            // Taken since `unique_path` looked; it skips that name now.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempts < 100 => attempts += 1,
            Err(e) => return Err(e.into()),
        }
    }
}

/// Size a fresh partial file for `total` bytes. A resumable download gets its
/// token at the end right away, so its file can be told apart from another
/// download's even after a crash; other downloads add it when they stop.
fn prepare_part(file: &File, dir: &Path, total: Option<u64>, resumable: bool, token: &[u8; TOKEN_LEN]) -> Result<()> {
    let Some(total) = total else { return Ok(()) };
    fileio::set_sparse(file, true);
    let sparse = is_sparse(file);
    if !sparse {
        // Without sparse support (FAT32, exFAT, some shares) the size the
        // server declares is allocated for real: don't let it claim more
        // than the disk holds.
        ensure_space(dir, total + TOKEN_LEN as u64)?;
    }
    if resumable {
        write_token(file, total, token)?;
    } else if sparse {
        // One connection writes from the start, so only sparse files gain
        // anything from being sized up front.
        file.set_len(total)?;
    }
    Ok(())
}

#[cfg(windows)]
fn is_sparse(file: &File) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE;
    file.metadata().is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0)
}

/// Unix filesystems create holes when a file is extended.
#[cfg(not(windows))]
fn is_sparse(_file: &File) -> bool {
    true
}

fn ensure_space(dir: &Path, needed: u64) -> Result<()> {
    match free_space(dir) {
        Some(available) if needed > available => Err(Error::DiskFull { needed, available }),
        _ => Ok(()),
    }
}

/// Bytes the current user may still write to the volume holding `dir`.
#[cfg(windows)]
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    // A share's root must end with a backslash; any other folder may.
    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    if wide.last() != Some(&u16::from(b'\\')) {
        wide.push(u16::from(b'\\'));
    }
    wide.push(0);
    let mut available = 0u64;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the two other
    // outputs are optional.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(available)
}

#[cfg(not(windows))]
fn free_space(_dir: &Path) -> Option<u64> {
    None
}

/// Shared write path of one download.
struct Writer {
    file: Arc<File>,
    /// In-flight batch bytes, in KiB.
    budget: Arc<Semaphore>,
    /// Recycled batch buffers (fresh allocations would page-fault).
    pool: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Writer {
    fn take_buf(&self) -> Vec<u8> {
        self.pool.lock().pop().unwrap_or_else(|| Vec::with_capacity(BUF_CAP))
    }
}

fn recycle(pool: &Mutex<Vec<Vec<u8>>>, mut buf: Vec<u8>) {
    let limit = (WRITE_BUDGET_KIB as usize * 1024) / BUF_CAP + 8;
    if buf.capacity() <= 2 * BUF_CAP {
        buf.clear();
        let mut p = pool.lock();
        if p.len() < limit {
            p.push(buf);
        }
    }
}

struct Ctx {
    url: Url,
    /// The URL the credentials in `headers` were collected for.
    origin: Url,
    headers: HeaderMap,
    /// Marks the partial file as this download's.
    token: [u8; TOKEN_LEN],
    writer: Writer,
    /// Second handle used for flushing, so a flush never blocks writers that
    /// are serialized on the main handle.
    sync_handle: Option<Arc<File>>,
    progress: Arc<Progress>,
    /// Stops all workers (user pause or fatal error).
    stop: CancellationToken,
    user_cancel: CancellationToken,
    opts: EngineOptions,
    info: FileInfo,
    etag: Option<String>,
    last_modified: Option<String>,
    /// Set when two connections delivered different bytes for one offset.
    corrupted: Arc<AtomicBool>,
    /// Bytes a connection received past its segment end, at file offsets
    /// whose owner had not committed them yet; checked when it does.
    overlaps: Mutex<Vec<(u64, Vec<u8>)>>,
    /// Error that ends the whole download (disk failure, file changed...).
    fatal: Mutex<Option<Error>>,
    /// Most recent per-connection error, reported if every connection fails.
    last_error: Mutex<Option<Error>>,
    /// Connections the server currently tolerates (AIMD: cut to what works
    /// on 429/503, raised by one after a quiet period).
    limit: AtomicUsize,
    /// Workers currently running.
    alive: AtomicUsize,
    /// Requests sent whose response headers haven't arrived yet.
    opening: AtomicUsize,
    throttle: Mutex<Throttle>,
}

#[derive(Default)]
struct Throttle {
    /// When the server last refused a connection.
    last: Option<Instant>,
    /// Don't open new connections before this (from `Retry-After`).
    not_before: Option<Instant>,
}

/// Per-host connection caps learned from throttling, reused by later
/// downloads from the same host for a while so they don't start with a
/// burst of refused connections.
static HOST_LIMITS: std::sync::LazyLock<Mutex<std::collections::HashMap<String, (usize, Instant)>>> =
    std::sync::LazyLock::new(Default::default);
const HOST_LIMIT_TTL: Duration = Duration::from_secs(15 * 60);
/// Quiet time before trying one more connection after being throttled.
const PROBE_UP_AFTER: Duration = Duration::from_secs(2);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

fn learned_limit(host: &str) -> Option<usize> {
    let mut map = HOST_LIMITS.lock();
    match map.get(host) {
        Some(&(n, at)) if at.elapsed() < HOST_LIMIT_TTL => Some(n),
        Some(_) => {
            map.remove(host);
            None
        }
        None => None,
    }
}

enum End {
    Done,
    Stopped,
}

/// How a response body stream ended, before its data is fully written.
enum Ended {
    /// Reached the end of our (possibly shrunk) segment.
    Done,
    Stopped,
    /// The server finished the body.
    Eof,
}

impl Ctx {
    fn worker_count(&self) -> usize {
        if !self.info.resumable {
            return 1;
        }
        let total = self.info.total.unwrap_or(0);
        let useful = (total / self.opts.min_split.max(1)).max(1);
        self.opts.connections.clamp(1, 64).min(useful as usize)
    }

    fn validator(&self) -> Option<&str> {
        self.etag.as_deref().or(self.last_modified.as_deref())
    }

    fn read_timeout(&self) -> Duration {
        head_timeout(&self.opts.net)
    }

    fn fail(&self, err: Error) {
        let mut f = self.fatal.lock();
        if f.is_none() {
            *f = Some(err);
        }
        self.stop.cancel();
    }

    /// Key for [`HOST_LIMITS`]: connection caps are per server endpoint.
    fn host(&self) -> String {
        let host = self.url.host_str().unwrap_or_default().to_ascii_lowercase();
        format!("{host}:{}", self.url.port_or_known_default().unwrap_or(0))
    }

    fn note_throttle(&self, retry_after: Option<Duration>) {
        let now = Instant::now();
        let mut t = self.throttle.lock();
        t.last = Some(now);
        if let Some(wait) = retry_after {
            let until = now + wait.min(MAX_RETRY_AFTER);
            t.not_before = Some(t.not_before.map_or(until, |nb| nb.max(until)));
        }
    }

    /// A worker that just finished a segment leaves if more workers are
    /// running than the server tolerates. Returns true if it must exit (its
    /// `alive` count has then already been taken off).
    fn retire_over_limit(&self) -> bool {
        let limit = self.limit.load(Ordering::SeqCst).max(1);
        self.alive.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |a| (a > limit).then(|| a - 1)).is_ok()
    }

    /// Open a connection for the unfinished part of `slot`.
    async fn open(&self, client: &reqwest::Client, slot: usize, id: usize) -> Result<Option<reqwest::Response>> {
        let (offset, end) = {
            let t = self.progress.table.lock();
            let seg = t.segment(slot);
            if t.owner(slot) != Some(id) || seg.is_complete() {
                return Ok(None);
            }
            (seg.pos(), seg.end)
        };
        let mut extra = HeaderMap::new();
        if self.info.resumable {
            let range = match (end, self.info.total) {
                (UNKNOWN_END, _) | (_, None) => format!("bytes={offset}-"),
                (end, Some(total)) => format!("bytes={offset}-{}", (end + OVERLAP).min(total) - 1),
            };
            extra.insert(header::RANGE, HeaderValue::from_str(&range).expect("a byte range is a valid header value"));
            if let Some(v) = self.validator().and_then(|v| HeaderValue::from_str(v).ok()) {
                extra.insert(header::IF_RANGE, v);
            }
        }
        let timeout = self.read_timeout();
        let send = http::get(client, &self.url, &self.origin, &self.headers, extra, self.opts.net.max_redirects, |_| Ok(()));
        let resp = tokio::select! {
            r = tokio::time::timeout(timeout, send) => r.map_err(|_| Error::Stalled(timeout.as_secs()))??,
            _ = self.stop.cancelled() => return Ok(None),
        };
        let etag_changed = |resp: &reqwest::Response| {
            matches!((&self.etag, header_str(resp.headers(), header::ETAG)), (Some(want), Some(got)) if want != got)
        };
        match resp.status() {
            StatusCode::PARTIAL_CONTENT if self.info.resumable => {
                let got = header_str(resp.headers(), header::CONTENT_RANGE).and_then(http::parse_content_range);
                match got {
                    Some((first, _, total)) if first == offset && (total.is_none() || total == self.info.total) => {}
                    Some((_, _, total)) if total.is_some() && total != self.info.total => return Err(Error::RemoteChanged),
                    _ => return Err(Error::Other("server returned the wrong byte range".into())),
                }
                if etag_changed(&resp) {
                    return Err(Error::RemoteChanged);
                }
                Ok(Some(resp))
            }
            // Restart of a non-resumable download: it must be the same file.
            StatusCode::OK if !self.info.resumable && offset == 0 => {
                let resized = matches!((self.info.total, resp.content_length()), (Some(a), Some(b)) if a != b);
                if resized || etag_changed(&resp) {
                    return Err(Error::RemoteChanged);
                }
                Ok(Some(resp))
            }
            // A full response to a conditional range request: the validator no
            // longer matches, i.e. the file changed.
            StatusCode::OK if self.validator().is_some() => Err(Error::RemoteChanged),
            StatusCode::OK => Err(Error::Other("server stopped honouring byte ranges".into())),
            StatusCode::RANGE_NOT_SATISFIABLE => Err(Error::RemoteChanged),
            s => Err(status_error(&resp, s)),
        }
    }

    async fn stream(
        &self,
        slot: usize,
        id: usize,
        mut resp: reqwest::Response,
        pipe: &mut Pipe,
        streamed: Option<&AtomicBool>,
    ) -> Result<End> {
        let _active = ActiveGuard::new(&self.progress.active);
        let read_timeout = self.read_timeout();
        let mut meter = SpeedMeter::new();
        let body_len = resp.content_length();
        let mut consumed = 0u64;

        let ended = loop {
            let next = tokio::select! {
                biased;
                _ = self.stop.cancelled() => break Ok(Ended::Stopped),
                r = tokio::time::timeout(read_timeout, resp.chunk()) => r,
            };
            let chunk = match next {
                Err(_) => break Err(Error::Stalled(read_timeout.as_secs())),
                Ok(Err(e)) => break Err(e.into()),
                Ok(Ok(None)) => break Ok(Ended::Eof),
                Ok(Ok(Some(c))) => c,
            };
            consumed += chunk.len() as u64;
            let want = chunk.len() as u64;
            let (offset, allowed) = self.progress.table.lock().reserve(slot, id, want);
            if allowed > 0 {
                if let Some(flag) = streamed {
                    flag.store(true, Ordering::Relaxed);
                }
                self.progress.received.fetch_add(allowed, Ordering::Relaxed);
                if let Some(speed) = meter.add(allowed) {
                    self.progress.table.lock().set_speed(slot, speed);
                }
                if let Err(e) = pipe.push(self, slot, offset, &chunk[..allowed as usize]).await {
                    break Err(e);
                }
                for limiter in &self.opts.limiters {
                    tokio::select! {
                        _ = limiter.acquire(allowed) => {}
                        _ = self.stop.cancelled() => {}
                    }
                }
            }
            if allowed < want {
                let mut beyond = chunk.slice(allowed as usize..);
                // If only the requested overlap is left, read it too: the
                // comparison gets all of it and the connection stays reusable.
                let rest = body_len.map(|n| n.saturating_sub(consumed));
                if rest.is_some_and(|r| r > 0 && r <= 2 * OVERLAP) {
                    let mut all = beyond.to_vec();
                    loop {
                        let next = tokio::select! {
                            biased;
                            _ = self.stop.cancelled() => break,
                            r = tokio::time::timeout(read_timeout, resp.chunk()) => r,
                        };
                        match next {
                            Ok(Ok(Some(c))) => all.extend_from_slice(&c),
                            _ => break,
                        }
                    }
                    beyond = all.into();
                }
                let end = self.progress.table.lock().segment(slot).end;
                if offset + allowed == end {
                    self.cross_check(end, &beyond);
                }
                break Ok(Ended::Done);
            }
        };

        // Whatever happened, everything received so far is written and
        // committed before the segment is looked at or handed on.
        let drained = pipe.drain(self, slot).await;
        match (ended, drained) {
            (_, Err(e)) => Err(e),
            (Err(e), Ok(())) => Err(e),
            (Ok(Ended::Done), Ok(())) => Ok(End::Done),
            (Ok(Ended::Stopped), Ok(())) => Ok(End::Stopped),
            (Ok(Ended::Eof), Ok(())) => self.on_eof(slot),
        }
    }

    fn on_eof(&self, slot: usize) -> Result<End> {
        let mut t = self.progress.table.lock();
        if t.segment(slot).is_complete() {
            Ok(End::Done)
        } else if self.info.total.is_none() {
            t.finish_unknown(slot);
            Ok(End::Done)
        } else {
            Err(Error::Truncated)
        }
    }

    /// Our stream ran past the end of our segment into bytes that another
    /// connection downloads too: they must be identical. Compare now if the
    /// other connection has committed them, otherwise when it does.
    fn cross_check(&self, pos: u64, beyond: &[u8]) {
        let beyond = &beyond[..beyond.len().min(CROSS_CHECK_MAX)];
        if beyond.is_empty() {
            return;
        }
        // Lock order overlaps -> table; committers take them one at a time,
        // so a commit is either visible here or sees our entry.
        let mut overlaps = self.overlaps.lock();
        let durable = self.progress.table.lock().durable_from(pos);
        if durable == 0 {
            overlaps.push((pos, beyond.to_vec()));
            return;
        }
        drop(overlaps);
        self.compare_on_disk(pos, &beyond[..beyond.len().min(durable as usize)]);
    }

    /// `[offset, offset + n)` was just committed: verify parked overlaps.
    fn check_overlaps(&self, offset: u64, n: u64) {
        let hits: Vec<(u64, Vec<u8>)> = {
            let mut overlaps = self.overlaps.lock();
            if overlaps.is_empty() {
                return;
            }
            let (hits, rest) = std::mem::take(&mut *overlaps)
                .into_iter()
                .partition(|(pos, _)| *pos >= offset && *pos < offset + n);
            *overlaps = rest;
            hits
        };
        for (pos, bytes) in hits {
            let len = bytes.len().min((offset + n - pos) as usize);
            self.compare_on_disk(pos, &bytes[..len]);
        }
    }

    fn compare_on_disk(&self, pos: u64, expected: &[u8]) {
        let mut on_disk = vec![0u8; expected.len()];
        if fileio::read_exact_at(&self.writer.file, &mut on_disk, pos).is_err() {
            return;
        }
        if on_disk != expected {
            tracing::warn!(offset = pos, len = expected.len(), "connections received different data for the same bytes");
            self.corrupted.store(true, Ordering::Relaxed);
            self.fail(Error::RemoteChanged);
        }
    }

    fn resume_data(&self, segments: Vec<Segment>) -> ResumeData {
        ResumeData {
            final_url: self.info.final_url.clone(),
            file_path: self.info.file_path.clone(),
            total: self.info.total,
            resumable: self.info.resumable,
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
            segments,
            part_token: Some(hex::encode(self.token)),
        }
    }

    fn publish_checkpoint(&self, segments: Vec<Segment>) {
        *self.progress.checkpoint.lock() = Some(self.resume_data(segments));
    }

    /// Snapshot committed progress, flush it to disk, then publish it.
    async fn checkpoint(&self) {
        let segments = self.progress.table.lock().snapshot();
        if let Some(h) = self.sync_handle.clone() {
            let _ = tokio::task::spawn_blocking(move || h.sync_data()).await;
        }
        self.publish_checkpoint(segments);
    }

    async fn final_checkpoint(&self) {
        let file = self.writer.file.clone();
        let (segments, written) = {
            let t = self.progress.table.lock();
            (t.snapshot(), t.written())
        };
        // A resumable file has carried its token from the start. Any other
        // gets it now, after the bytes written from its start: there it costs
        // nothing even where files can't be sparse, and lets the next run
        // tell that this file is its own to delete.
        let mark = (!self.info.resumable).then_some((written, self.token));
        let _ = tokio::task::spawn_blocking(move || {
            if let Some((at, token)) = mark {
                let _ = write_token(&file, at, &token);
            }
            file.sync_data()
        })
        .await;
        self.publish_checkpoint(segments);
    }
}

/// One connection's write queue: the batch being filled plus the writes in
/// flight, committed strictly in order.
struct Pipe {
    buf: Vec<u8>,
    offset: u64,
    /// When the first byte of the current batch arrived.
    since: Instant,
    /// `(offset, len, write)` of batches in flight.
    pending: VecDeque<(u64, u64, JoinHandle<std::io::Result<()>>)>,
}

impl Pipe {
    fn new() -> Self {
        Self { buf: Vec::new(), offset: 0, since: Instant::now(), pending: VecDeque::new() }
    }

    async fn push(&mut self, ctx: &Ctx, slot: usize, offset: u64, bytes: &[u8]) -> Result<()> {
        if !self.buf.is_empty() && self.buf.len() + bytes.len() > BUF_CAP {
            self.flush(ctx, slot).await?;
        }
        if self.buf.is_empty() {
            if self.buf.capacity() == 0 {
                self.buf = ctx.writer.take_buf();
            }
            self.offset = offset;
            self.since = Instant::now();
        }
        debug_assert_eq!(self.offset + self.buf.len() as u64, offset, "batch must stay contiguous");
        self.buf.extend_from_slice(bytes);
        if self.buf.len() >= FLUSH_AT || self.since.elapsed() >= FLUSH_AFTER {
            self.flush(ctx, slot).await?;
        }
        Ok(())
    }

    /// Start writing the current batch (waiting for budget if the disk is
    /// behind) and commit any writes that have completed meanwhile.
    async fn flush(&mut self, ctx: &Ctx, slot: usize) -> Result<()> {
        self.reap(ctx, slot, false).await?;
        if self.buf.is_empty() {
            return Ok(());
        }
        let kib = (self.buf.len().div_ceil(1024) as u32).min(WRITE_BUDGET_KIB);
        let budget = ctx.writer.budget.clone();
        let permit: OwnedSemaphorePermit = match budget.clone().try_acquire_many_owned(kib) {
            Ok(p) => p,
            // The disk is behind. Wait until half the budget is free rather
            // than for every single batch: each pause stalls the TCP stream
            // and restarting it costs far more than the pause itself.
            Err(_) => {
                let hysteresis = WRITE_BUDGET_KIB / 2;
                let mut p = budget
                    .acquire_many_owned((kib + hysteresis).min(WRITE_BUDGET_KIB))
                    .await
                    .expect("budget semaphore is never closed");
                drop(p.split(p.num_permits() - kib as usize));
                p
            }
        };
        let data = std::mem::replace(&mut self.buf, ctx.writer.take_buf());
        let n = data.len() as u64;
        let (file, pool, offset) = (ctx.writer.file.clone(), ctx.writer.pool.clone(), self.offset);
        let handle = tokio::task::spawn_blocking(move || {
            let written = fileio::write_all_at(&file, &data, offset);
            recycle(&pool, data);
            drop(permit);
            written
        });
        self.pending.push_back((offset, n, handle));
        self.reap(ctx, slot, false).await
    }

    /// Commit completed writes in order; with `all`, wait for every one.
    async fn reap(&mut self, ctx: &Ctx, slot: usize, all: bool) -> Result<()> {
        let mut failed = None;
        while let Some((_, _, h)) = self.pending.front() {
            if !all && failed.is_none() && !h.is_finished() {
                break;
            }
            let (offset, n, h) = self.pending.pop_front().expect("front exists");
            let result = h.await.unwrap_or_else(|e| Err(std::io::Error::other(e.to_string())));
            match result {
                Ok(()) if failed.is_none() => {
                    ctx.progress.table.lock().commit(slot, n);
                    ctx.check_overlaps(offset, n);
                }
                Ok(()) => {}
                Err(e) => {
                    failed.get_or_insert(e);
                }
            }
        }
        match failed {
            None => Ok(()),
            Some(e) => {
                let e = Error::Io(e);
                let msg = e.to_string();
                ctx.fail(e);
                Err(Error::Other(msg))
            }
        }
    }

    /// Write out the partial batch and wait until everything is committed.
    async fn drain(&mut self, ctx: &Ctx, slot: usize) -> Result<()> {
        let flushed = self.flush(ctx, slot).await;
        let reaped = self.reap(ctx, slot, true).await;
        flushed.and(reaped)
    }
}

/// One connection: claims segments and streams them until nothing is left.
/// `streamed` (for replacement connections) is set once data arrives.
async fn worker(
    ctx: Arc<Ctx>,
    id: usize,
    client: reqwest::Client,
    mut initial: Option<(usize, reqwest::Response)>,
    streamed: Option<Arc<AtomicBool>>,
) {
    ctx.alive.fetch_add(1, Ordering::SeqCst);
    let mut alive = AliveGuard { ctx: &ctx, counted: true };
    let mut pipe = Pipe::new();
    let mut slot = match &initial {
        Some((s, _)) => Some(*s),
        None => ctx.progress.table.lock().claim(id),
    };

    while let Some(current) = slot {
        let mut failures = 0u32;
        let outcome = loop {
            if ctx.stop.is_cancelled() {
                break Step::Exit;
            }
            let before = ctx.progress.table.lock().segment(current).pos();
            let resp = match initial.take() {
                Some((_, r)) => Ok(Some(r)),
                None => {
                    ctx.opening.fetch_add(1, Ordering::SeqCst);
                    let r = ctx.open(&client, current, id).await;
                    ctx.opening.fetch_sub(1, Ordering::SeqCst);
                    r
                }
            };
            let result = match resp {
                Ok(None) => Ok(End::Done),
                Ok(Some(resp)) => ctx.stream(current, id, resp, &mut pipe, streamed.as_deref()).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(End::Done) => break Step::Next,
                Ok(End::Stopped) => break Step::Exit,
                Err(e) => {
                    if ctx.stop.is_cancelled() {
                        break Step::Exit;
                    }
                    if matches!(e, Error::RemoteChanged | Error::Io(_)) {
                        ctx.fail(e);
                        break Step::Exit;
                    }
                    tracing::debug!(worker = id, error = %e, "connection error");
                    // Progress only counts if it survives the retry: a
                    // non-resumable download starts over from zero.
                    let progressed =
                        ctx.info.resumable && ctx.progress.table.lock().segment(current).pos() > before;
                    failures = if progressed { 1 } else { failures + 1 };

                    // The server refuses more connections. If anyone else is
                    // still working, cut the limit to what currently works,
                    // fold our untouched range back into the neighbour that is
                    // streaming it and leave; never hold a segment hostage
                    // while backing off.
                    let mut throttle_wait = None;
                    if let Error::Throttled { retry_after, .. } = &e {
                        ctx.note_throttle(*retry_after);
                        let mut t = ctx.progress.table.lock();
                        if t.other_owners(id) > 0 {
                            // Connections that work (streaming) or may still
                            // work (response pending); the minimum over all
                            // refusals converges on the server's cap.
                            let working = ctx.progress.active_connections() + ctx.opening.load(Ordering::SeqCst);
                            ctx.limit.fetch_min(working.max(1), Ordering::SeqCst);
                            t.give_back(current, id);
                            drop(t);
                            tracing::debug!(worker = id, working, "throttled by server, retiring");
                            *ctx.last_error.lock() = Some(e);
                            return;
                        }
                        ctx.limit.store(1, Ordering::SeqCst);
                        throttle_wait = Some(retry_after.unwrap_or_default().min(MAX_RETRY_AFTER).max(backoff(failures)));
                    }

                    // When other connections are healthy, a failing one most
                    // likely hit a per-server connection limit: hand the
                    // segment back and retire instead of failing everything.
                    if failures >= 2 && retire_if_others_active(&ctx, current, id) {
                        *ctx.last_error.lock() = Some(e);
                        return;
                    }
                    if !e.is_transient() || failures > ctx.opts.max_retries {
                        *ctx.last_error.lock() = Some(e);
                        if !retire_if_others_active(&ctx, current, id) {
                            ctx.stop.cancel();
                        }
                        return;
                    }
                    *ctx.last_error.lock() = Some(e);
                    if !ctx.info.resumable {
                        let lost = ctx.progress.table.lock().reset(current);
                        ctx.progress.received.fetch_sub(lost, Ordering::Relaxed);
                    }
                    // A connection that was delivering data reconnects at
                    // once; one that cannot get going backs off.
                    if !progressed || throttle_wait.is_some() {
                        let wait = throttle_wait.unwrap_or_else(|| backoff(failures));
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => {}
                            _ = ctx.stop.cancelled() => break Step::Exit,
                        }
                    }
                }
            }
        };
        let mut t = ctx.progress.table.lock();
        t.release(current, id);
        slot = match outcome {
            Step::Next if !ctx.stop.is_cancelled() => {
                if ctx.retire_over_limit() {
                    alive.counted = false;
                    None
                } else {
                    t.claim(id)
                }
            }
            _ => None,
        };
    }
}

struct AliveGuard<'a> {
    ctx: &'a Ctx,
    counted: bool,
}

impl Drop for AliveGuard<'_> {
    fn drop(&mut self) {
        if self.counted {
            self.ctx.alive.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

enum Step {
    Next,
    Exit,
}

/// Atomically: if another connection is still working, give `slot` back so
/// that connection picks it up when it finishes.
fn retire_if_others_active(ctx: &Ctx, slot: usize, id: usize) -> bool {
    let mut t = ctx.progress.table.lock();
    let others = t.other_owners(id) > 0;
    if others {
        t.release(slot, id);
    }
    others
}

async fn checkpoint_loop(ctx: Arc<Ctx>, stop: CancellationToken) {
    let mut tick = tokio::time::interval(ctx.opts.checkpoint_interval.max(Duration::from_millis(10)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = tick.tick() => ctx.checkpoint().await,
        }
    }
}

async fn finish(ctx: Arc<Ctx>) -> Result<Outcome> {
    let progress = ctx.progress.clone();
    progress.set_phase(Phase::Finishing);
    let size = ctx.info.total.unwrap_or_else(|| progress.table.lock().extent());
    let expected = ctx.opts.expected_sha256.clone();
    let file_path = ctx.info.file_path.clone();
    let part_path = naming::with_suffix(&file_path, PART_SUFFIX);
    let token = ctx.token;
    let start_over = ctx.resume_data(Vec::new()).start_over();
    let host_url = ctx.info.final_url.clone();
    let referrer = header_str(&ctx.headers, header::REFERER).map(str::to_string);

    let file = ctx.writer.file.clone();
    // Cutting the file to its size also drops the token.
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        file.set_len(size)?;
        file.sync_all()?;
        fileio::set_sparse(&file, false);
        Ok(())
    })
    .await
    .map_err(|e| Error::Other(e.to_string()))??;
    drop(ctx); // closes every handle so the file can be renamed

    // Verify before the rename: a file that fails must never sit under its
    // final name, where it looks like a finished download.
    if let Some(expected) = expected {
        progress.set_phase(Phase::Verifying);
        let path = part_path.clone();
        let actual = tokio::task::spawn_blocking(move || sha256_file(&path))
            .await
            .map_err(|e| Error::Other(e.to_string()))??;
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            let _ = std::fs::remove_file(&part_path);
            *progress.checkpoint.lock() = Some(start_over);
            return Err(Error::Checksum { expected, actual });
        }
    }

    // As browsers do, so that SmartScreen, Office and the like treat the file
    // as coming from the internet. Volumes without alternate data streams
    // (FAT32, exFAT) can't hold the mark; the download stands anyway.
    if let Err(e) = fileio::mark_from_internet(&part_path, &host_url, referrer.as_deref()) {
        tracing::info!(path = %part_path.display(), error = %e, "cannot record where the file came from");
    }

    let target = if file_path.exists() {
        let dir = file_path.parent().map(PathBuf::from).unwrap_or_default();
        let name = file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        naming::unique_path(&dir, &name, PART_SUFFIX)
    } else {
        file_path.clone()
    };
    if let Err(e) = rename_with_retry(&part_path, &target).await {
        // The (complete) checkpoint is kept, so a resume only has to retry
        // this step; it needs the token back to recognise the file.
        let _ = fileio::open_partial(&part_path).and_then(|f| write_token(&f, size, &token));
        return Err(e.into());
    }
    *progress.checkpoint.lock() = None;
    if target != file_path
        && let Some(info) = progress.info.lock().as_mut()
    {
        info.file_name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        info.file_path = target.clone();
    }
    Ok(Outcome::Completed { path: target, size })
}

/// Scanners and indexers often open a freshly written file for a moment;
/// on Windows that makes the rename fail with a sharing violation.
async fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut attempt = 0;
    loop {
        match std::fs::rename(from, to) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(32) => {
                attempt += 1;
                if attempt >= 10 {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_millis(100 * attempt)).await;
            }
            other => return other,
        }
    }
}

pub fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

struct ActiveGuard<'a>(&'a AtomicUsize);

impl<'a> ActiveGuard<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Exponentially smoothed per-connection throughput, sampled every 500 ms.
struct SpeedMeter {
    since: Instant,
    bytes: u64,
    ewma: f64,
}

impl SpeedMeter {
    fn new() -> Self {
        Self { since: Instant::now(), bytes: 0, ewma: 0.0 }
    }

    fn add(&mut self, n: u64) -> Option<f64> {
        self.bytes += n;
        let dt = self.since.elapsed().as_secs_f64();
        if dt < 0.5 {
            return None;
        }
        let inst = self.bytes as f64 / dt;
        self.ewma = if self.ewma == 0.0 { inst } else { 0.6 * self.ewma + 0.4 * inst };
        self.bytes = 0;
        self.since = Instant::now();
        Some(self.ewma)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let fut = run(
            DownloadRequest::default(),
            EngineOptions::default(),
            None,
            Arc::new(Progress::default()),
            CancellationToken::new(),
        );
        assert_send(&fut);
    }

    #[test]
    fn resume_maps_must_tile_the_file() {
        let s = |start, end, done| Segment { start, end, done };
        assert!(segments_tile(&[s(5, 10, 0), s(0, 5, 5)], 10));
        assert!(segments_tile(&[], 0));
        assert!(!segments_tile(&[s(0, 5, 5)], 10), "gap at the end");
        assert!(!segments_tile(&[s(0, 6, 0), s(5, 10, 0)], 10), "overlap");
        assert!(!segments_tile(&[s(0, 10, 11)], 10), "done beyond the segment");
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert_eq!(backoff(1), Duration::from_millis(250));
        assert_eq!(backoff(2), Duration::from_millis(500));
        assert!(backoff(5) < backoff(6));
        assert_eq!(backoff(40), Duration::from_secs(30));
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spate-download-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_a_part_file_with_our_token_is_discarded() {
        let dir = temp_dir("token");
        let mine = new_token().unwrap();
        let r = ResumeData {
            final_url: "https://example.com/f.bin".into(),
            file_path: dir.join("f.bin"),
            total: Some(10),
            resumable: true,
            etag: None,
            last_modified: None,
            segments: vec![Segment { start: 0, end: 10, done: 4 }],
            part_token: Some(hex::encode(mine)),
        };
        let mark = |token: &[u8; TOKEN_LEN]| {
            let f = OpenOptions::new().write(true).create(true).truncate(true).open(r.part_path()).unwrap();
            write_token(&f, 10, token).unwrap();
        };
        // Another download's file under the same name.
        mark(&new_token().unwrap());
        assert!(!discard_part(&r));
        assert!(!discard_part(&r.start_over()), "a fresh start claims no file");
        assert!(r.part_path().exists());
        mark(&mine);
        assert_eq!(std::fs::metadata(r.part_path()).unwrap().len(), 10 + TOKEN_LEN as u64);
        assert!(!discard_part(&r.start_over()));
        assert!(discard_part(&r));
        assert!(!r.part_path().exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resume_data_without_a_token_still_loads() {
        let json = r#"{"final_url":"https://example.com/f.bin","file_path":"f.bin","total":10,"resumable":true,
            "etag":null,"last_modified":null,"segments":[{"start":0,"end":10,"done":4}]}"#;
        let r: ResumeData = serde_json::from_str(json).unwrap();
        assert_eq!((r.written(), r.part_token), (4, None));
    }

    #[cfg(windows)]
    #[test]
    fn declared_sizes_must_fit_on_the_volume() {
        let dir = temp_dir("space");
        ensure_space(&dir, 1).unwrap();
        match ensure_space(&dir, u64::MAX / 2) {
            Err(e @ Error::DiskFull { available, .. }) => {
                assert!(available > 0);
                assert!(e.to_string().starts_with("not enough disk space: the file needs 8388608.0 TiB"), "{e}");
            }
            other => panic!("expected a disk-full error, got {other:?}"),
        }
        // Temp folders are on NTFS, where the fast path applies.
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(dir.join("s.part")).unwrap();
        fileio::set_sparse(&f, true);
        assert!(is_sparse(&f));
        drop(f);
        std::fs::remove_dir_all(&dir).ok();
    }
}
