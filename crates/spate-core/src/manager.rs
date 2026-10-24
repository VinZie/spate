//! The download manager: list of downloads, queue/scheduler, persistence.
//!
//! CONTRACT: the public types and method signatures in this file are shared
//! with the GUI (`crates/spate`) and CLI (`crates/spate-cli`). Do not
//! change them without updating those crates.
//!
//! # How it works
//!
//! * All mutable state lives in one `parking_lot::Mutex<State>`. It is never
//!   held across an `.await`, and the user callbacks are only invoked after it
//!   has been released.
//! * Every running download is a task on the runtime handle passed to
//!   [`Manager::new`]. Stopping one (pause, queue stop, remove, redownload,
//!   shutdown) records *why* and cancels its token; the task's completion
//!   handler then applies the outcome to the record.
//! * A background ticker (every 500 ms) samples speeds, mirrors engine
//!   progress and resume checkpoints into the records, runs the queue and the
//!   daily schedule, and autosaves `downloads.json` (debounced, atomic).
//!
//! # Queue semantics
//!
//! * A record belongs to the queue when `queued` is set. Queue members wait in
//!   [`Status::Queued`]; while the queue runs, the oldest waiting members are
//!   started whenever fewer than `settings.max_parallel` downloads are active.
//! * Stopping the queue (manually or at the scheduled stop time) also stops the
//!   downloads the queue itself started and puts them back to `Queued`, so
//!   they continue the next time the queue runs.
//! * Starting the queue puts paused or failed members back to `Queued`.
//! * A queue download that fails is retried a few times with back-off before
//!   it is marked [`Status::Failed`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::{Condvar, Mutex};
use reqwest::header;
use serde::{Deserialize, Serialize};
use tokio::runtime::RuntimeFlavor;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::category::Category;
use crate::download::{self, DownloadRequest, EngineOptions, FileInfo, Outcome, PART_SUFFIX, Phase, Progress, ResumeData};
use crate::error::{Error, Result};
use crate::http::{self, NetConfig, header_str};
use crate::limiter::RateLimiter;
use crate::naming;
use crate::paths;
use crate::segments::SegmentView;
use crate::settings::{Schedule, Settings};

/// The download list.
const DOWNLOADS_FILE: &str = "downloads.json";
/// Where an unreadable download list is moved before starting empty.
const DOWNLOADS_BACKUP: &str = "downloads.json.bak";
/// Small sidecar holding the id counter, so ids are never reused.
const META_FILE: &str = "downloads.meta.json";

const TICK: Duration = Duration::from_millis(500);
/// Autosave delay after a visible state change.
const SAVE_SOON: Duration = Duration::from_secs(1);
/// Autosave delay after a new resume checkpoint of a running download.
const SAVE_PROGRESS: Duration = Duration::from_secs(5);
const SAVE_RETRY: Duration = Duration::from_secs(10);
/// Time constant of the exponentially smoothed speed.
const SPEED_TAU_SECS: f64 = 2.0;
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const REMOVE_WAIT: Duration = Duration::from_millis(1500);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Automatic retries of a failed queue download.
const QUEUE_RETRIES: u32 = 3;
const QUEUE_RETRY_BASE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DownloadKind {
    #[default]
    Http,
    /// HLS stream (`.m3u8`), saved as MPEG-TS (or `.mp4` for fMP4 streams).
    Hls,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Status {
    /// Waiting for the queue.
    Queued,
    Connecting,
    Downloading,
    /// Writing the last bytes / renaming / verifying.
    Finishing,
    Paused,
    Completed,
    Failed,
}

impl Status {
    pub fn is_active(self) -> bool {
        matches!(self, Status::Connecting | Status::Downloading | Status::Finishing)
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Queued => "Queued",
            Status::Connecting => "Connecting",
            Status::Downloading => "Downloading",
            Status::Finishing => "Finishing",
            Status::Paused => "Paused",
            Status::Completed => "Complete",
            Status::Failed => "Error",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartMode {
    /// Start immediately, regardless of the queue's parallel limit.
    #[default]
    Now,
    /// Add to the queue; it starts when the queue runs and a slot is free.
    Queue,
    /// Add without starting.
    Paused,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AddRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub file_name: Option<String>,
    /// `None` = `settings.dir_for(category of the file name)`.
    pub dir: Option<PathBuf>,
    pub start: StartMode,
    /// `None` = `settings.connections`.
    pub connections: Option<usize>,
    /// Per-download limit in bytes/s; `None` or 0 = unlimited.
    pub speed_limit: Option<u64>,
    pub sha256: Option<String>,
    /// `None` = detect (`.m3u8` URL or HLS MIME type => Hls).
    pub kind: Option<DownloadKind>,
}

/// Persisted record of one download (`downloads.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DownloadRecord {
    pub id: u64,
    pub url: String,
    /// Request headers. Credentials among them (cookies, `Authorization`,
    /// see [`is_credential_header`]) live in memory for this session only and
    /// are never written to `downloads.json`: the browser keeps cookies
    /// encrypted, and a plain-text copy would undo that against infostealers.
    /// A download that needed them and is resumed after a restart goes on
    /// without them, which may fail; sending it again from the browser
    /// supplies fresh ones.
    #[serde(serialize_with = "serialize_without_credentials")]
    pub headers: Vec<(String, String)>,
    pub kind: DownloadKind,
    pub file_name: String,
    /// `file_name` was chosen (confirmed in a dialog, given on the command
    /// line or by the browser), so the server's suggestion must not
    /// replace it.
    #[serde(default)]
    pub name_chosen: bool,
    pub dir: PathBuf,
    /// Final path once known (after the first response).
    pub file_path: Option<PathBuf>,
    pub category: Category,
    pub status: Status,
    pub total: Option<u64>,
    pub downloaded: u64,
    pub connections: usize,
    pub speed_limit: Option<u64>,
    pub sha256: Option<String>,
    pub resume: Option<ResumeData>,
    pub error: Option<String>,
    /// Unix seconds.
    pub added: u64,
    pub finished: Option<u64>,
    pub queued: bool,
}

/// What the UI shows for one download.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemView {
    pub id: u64,
    pub url: String,
    pub kind: DownloadKind,
    pub file_name: String,
    pub dir: PathBuf,
    pub file_path: Option<PathBuf>,
    pub category: Category,
    pub status: Status,
    pub total: Option<u64>,
    pub downloaded: u64,
    /// Smoothed bytes/s.
    pub speed: f64,
    pub eta: Option<Duration>,
    pub active_connections: usize,
    pub connections: usize,
    pub resumable: Option<bool>,
    pub error: Option<String>,
    pub added: u64,
    pub finished: Option<u64>,
    pub queued: bool,
}

impl ItemView {
    pub fn fraction(&self) -> Option<f32> {
        self.total.filter(|&t| t > 0).map(|t| (self.downloaded as f64 / t as f64).clamp(0.0, 1.0) as f32)
    }
}

/// Result of a quick look at a URL before adding it.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeInfo {
    pub file_name: String,
    pub total: Option<u64>,
    pub resumable: bool,
    pub content_type: Option<String>,
    pub final_url: String,
    pub kind: DownloadKind,
}

/// Cheap to clone; all clones share the same state.
#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

type ChangeFn = Arc<dyn Fn() + Send + Sync>;
type CompleteFn = Arc<dyn Fn(u64, PathBuf) + Send + Sync>;

struct Inner {
    rt: tokio::runtime::Handle,
    data_dir: PathBuf,
    state: Mutex<State>,
    /// Signalled (with `state` locked) whenever a download task has ended.
    task_ended: Condvar,
    global_limiter: Arc<RateLimiter>,
    on_change: Mutex<Option<ChangeFn>>,
    on_complete: Mutex<Option<CompleteFn>>,
    /// Serialises writers of the data files so an older snapshot can never
    /// overwrite a newer one.
    save_lock: Mutex<()>,
    stop_ticker: CancellationToken,
}

struct State {
    settings: Settings,
    /// Ordered by id, i.e. oldest first.
    records: Vec<DownloadRecord>,
    next_id: u64,
    running: HashMap<u64, Running>,
    queue_running: bool,
    /// Automatic retry bookkeeping of failed queue downloads.
    retries: HashMap<u64, Retry>,
    /// When the next autosave is due, if anything changed.
    save_due: Option<Instant>,
    /// Local minute-of-day seen by the previous schedule check.
    sched_prev: Option<u32>,
    next_gen: u64,
    shutting_down: bool,
}

/// A download whose engine task is alive.
struct Running {
    /// Distinguishes successive runs of the same record.
    generation: u64,
    progress: Arc<Progress>,
    cancel: CancellationToken,
    /// Per-download limiter (rate 0 = unlimited); adjustable while running.
    limiter: Arc<RateLimiter>,
    /// Started by the queue (so stopping the queue stops it too).
    by_queue: bool,
    /// Why it was asked to stop, if it was.
    stop: Option<StopReason>,
    /// Files to delete once a removed download has stopped.
    remove_files: Vec<PathBuf>,
    /// The removed record's resume point: it proves which partial file is its own.
    remove_claim: Option<ResumeData>,
    speed: f64,
    last_received: u64,
    last_sample: Instant,
    /// Whether `last_received` is a valid baseline for speed sampling.
    baseline: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    Pause,
    /// Stopped by the queue: back to `Queued`.
    Requeue,
    /// Redownload from scratch once stopped.
    Restart,
    /// Resumed while a pause was still in progress: start again once stopped.
    Continue,
    Remove { delete_file: bool },
    /// App exit: keep the active status so the next launch knows it was active.
    Shutdown,
}

struct Retry {
    attempts: u32,
    not_before: Instant,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Meta {
    next_id: u64,
}

#[derive(Default)]
struct TickResult {
    save: bool,
    notify: bool,
}

impl State {
    fn record(&self, id: u64) -> Option<&DownloadRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    fn record_mut(&mut self, id: u64) -> Option<&mut DownloadRecord> {
        self.records.iter_mut().find(|r| r.id == id)
    }

    /// Schedule an autosave no later than `within` from now.
    fn mark_dirty(&mut self, within: Duration) {
        let due = Instant::now() + within;
        self.save_due = Some(self.save_due.map_or(due, |d| d.min(due)));
    }

    /// Ask a running download to stop. Returns `false` if it is not running.
    fn request_stop(&mut self, id: u64, reason: StopReason) -> bool {
        let Some(run) = self.running.get_mut(&id) else { return false };
        run.stop = match (run.stop, reason) {
            // Removal is final and wins over everything.
            (_, StopReason::Remove { .. }) => Some(reason),
            (Some(StopReason::Remove { .. }), _) => run.stop,
            // Exit does not override an explicit user request (a paused
            // download must not be resumed on the next launch).
            (Some(_), StopReason::Shutdown) => run.stop,
            (Some(StopReason::Shutdown), _) => run.stop,
            // The queue stopping does not override what the user asked for.
            (Some(StopReason::Pause | StopReason::Restart | StopReason::Continue), StopReason::Requeue) => run.stop,
            _ => Some(reason),
        };
        run.cancel.cancel();
        true
    }

    fn stop_queue(&mut self) {
        self.queue_running = false;
        let ids: Vec<u64> = self.running.iter().filter(|(_, r)| r.by_queue).map(|(id, _)| *id).collect();
        for id in ids {
            self.request_stop(id, StopReason::Requeue);
        }
    }
}

impl Manager {
    /// Load `downloads.json` / `settings.json` from `data_dir` and start the
    /// background scheduler (speed sampling, queue, schedule, autosave) on `rt`.
    /// Records left `Downloading` by a previous run become `Paused` (or are
    /// resumed if `settings.resume_on_launch`).
    ///
    /// Queue members that were active go back to `Queued` instead, so the
    /// queue continues them.
    pub fn new(rt: tokio::runtime::Handle, data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        let settings = Settings::load(&data_dir);
        let mut records = load_records(&data_dir);
        records.sort_by_key(|r| r.id);
        records.dedup_by_key(|r| r.id);
        let meta = load_meta(&data_dir);
        let next_id = records.iter().map(|r| r.id + 1).max().unwrap_or(1).max(meta.next_id).max(1);

        let mut resume_now = Vec::new();
        let mut converted = false;
        for rec in &mut records {
            // Lists saved by older versions still hold credentials; the next
            // save drops them from the file.
            if rec.headers.iter().any(|(name, _)| is_credential_header(name)) {
                rec.headers.retain(|(name, _)| !is_credential_header(name));
                converted = true;
            }
            if rec.status.is_active() {
                if settings.resume_on_launch {
                    resume_now.push((rec.id, rec.queued));
                }
                rec.status = if rec.queued { Status::Queued } else { Status::Paused };
                converted = true;
            } else if rec.status == Status::Queued && !rec.queued {
                rec.status = Status::Paused;
                converted = true;
            }
        }

        let minute = clock::local_minutes();
        let queue_running = settings.start_queue_on_launch || schedule_window_open(&settings.schedule, minute);
        let global_limiter = Arc::new(RateLimiter::new(settings.speed_limit_kib.saturating_mul(1024)));
        let inner = Arc::new(Inner {
            rt,
            data_dir,
            state: Mutex::new(State {
                settings,
                records,
                next_id,
                running: HashMap::new(),
                queue_running,
                retries: HashMap::new(),
                save_due: None,
                sched_prev: Some(minute),
                next_gen: 0,
                shutting_down: false,
            }),
            task_ended: Condvar::new(),
            global_limiter,
            on_change: Mutex::new(None),
            on_complete: Mutex::new(None),
            save_lock: Mutex::new(()),
            stop_ticker: CancellationToken::new(),
        });
        {
            let mut st = inner.state.lock();
            if converted {
                st.mark_dirty(SAVE_SOON);
            }
            for (id, queued) in resume_now {
                inner.start_locked(&mut st, id, queued);
            }
            inner.pump_queue(&mut st);
        }
        inner.rt.spawn(ticker(Arc::downgrade(&inner), inner.stop_ticker.clone()));
        Ok(Self { inner })
    }

    /// Called (from any thread) whenever visible state changes, e.g. to
    /// request a UI repaint.
    pub fn set_on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.inner.on_change.lock() = Some(Arc::new(f));
    }

    /// Called when a download completes (id, final path), e.g. for a
    /// desktop notification.
    pub fn set_on_complete(&self, f: impl Fn(u64, PathBuf) + Send + Sync + 'static) {
        *self.inner.on_complete.lock() = Some(Arc::new(f));
    }

    pub fn add(&self, req: AddRequest) -> u64 {
        let inner = &self.inner;
        let url = req.url.trim().to_string();
        let kind = req.kind.unwrap_or_else(|| detect_kind(&url));
        let chosen = req.file_name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(naming::sanitize);
        let name_chosen = chosen.is_some();
        let file_name = chosen.unwrap_or_else(|| default_name(&url, kind));
        let category = Category::for_file_name(&file_name);
        let queued = req.start == StartMode::Queue;
        let id = {
            let mut st = inner.state.lock();
            let id = st.next_id;
            st.next_id += 1;
            let dir = req.dir.clone().unwrap_or_else(|| st.settings.dir_for(category));
            let connections = req.connections.unwrap_or(st.settings.connections).clamp(1, 32);
            st.records.push(DownloadRecord {
                id,
                url,
                headers: req.headers,
                kind,
                file_name,
                name_chosen,
                dir,
                file_path: None,
                category,
                status: if queued { Status::Queued } else { Status::Paused },
                total: None,
                downloaded: 0,
                connections,
                speed_limit: req.speed_limit.filter(|&l| l > 0),
                sha256: req.sha256.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()),
                resume: None,
                error: None,
                added: unix_now(),
                finished: None,
                queued,
            });
            st.mark_dirty(SAVE_SOON);
            match req.start {
                StartMode::Now => {
                    inner.start_locked(&mut st, id, false);
                }
                StartMode::Queue => {
                    inner.pump_queue(&mut st);
                }
                StartMode::Paused => {}
            }
            id
        };
        inner.notify_change();
        id
    }

    /// Look up name/size/range support of a URL without adding it. The
    /// receiver resolves once the probe completes.
    pub fn probe(
        &self,
        url: String,
        headers: Vec<(String, String)>,
    ) -> tokio::sync::oneshot::Receiver<Result<ProbeInfo>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let net = self.inner.state.lock().settings.net.clone();
        self.inner.rt.spawn(async move {
            let result = match tokio::time::timeout(PROBE_TIMEOUT, probe_url(&url, &headers, &net)).await {
                Ok(r) => r,
                Err(_) => Err(Error::Other(format!("no response within {} seconds", PROBE_TIMEOUT.as_secs()))),
            };
            let _ = tx.send(result);
        });
        rx
    }

    pub fn resume(&self, id: u64) {
        let started = {
            let mut st = self.inner.state.lock();
            st.retries.remove(&id);
            if let Some(run) = st.running.get_mut(&id) {
                // Still winding down from a pause: continue right after.
                if matches!(run.stop, Some(StopReason::Pause | StopReason::Requeue)) {
                    run.stop = Some(StopReason::Continue);
                }
                return;
            }
            match st.record(id).map(|r| r.status) {
                Some(Status::Paused | Status::Failed | Status::Queued) => self.inner.start_locked(&mut st, id, false),
                _ => false,
            }
        };
        if started {
            self.inner.notify_change();
        }
    }

    pub fn pause(&self, id: u64) {
        let changed = {
            let mut guard = self.inner.state.lock();
            let st = &mut *guard;
            if st.request_stop(id, StopReason::Pause) {
                true
            } else if let Some(rec) = st.record_mut(id).filter(|r| r.status == Status::Queued) {
                rec.status = Status::Paused;
                st.mark_dirty(SAVE_SOON);
                true
            } else {
                false
            }
        };
        if changed {
            self.inner.notify_change();
        }
    }

    /// Remove from the list; with `delete_file` also delete the downloaded
    /// (or partial) file from disk.
    pub fn remove(&self, id: u64, delete_file: bool) {
        let inner = &self.inner;
        let (running, to_delete, claims) = {
            let mut guard = inner.state.lock();
            let st = &mut *guard;
            let Some(idx) = st.records.iter().position(|r| r.id == id) else { return };
            let rec = st.records.remove(idx);
            st.retries.remove(&id);
            st.mark_dirty(SAVE_SOON);
            let files = if delete_file { owned_files(&rec, &paths_in_use(st, id), true) } else { Vec::new() };
            if st.request_stop(id, StopReason::Remove { delete_file }) {
                // Deleted by the task's completion handler once its file
                // handles are closed.
                if let Some(run) = st.running.get_mut(&id) {
                    run.remove_files = files;
                    run.remove_claim = rec.resume.clone();
                }
                (true, Vec::new(), Vec::new())
            } else {
                (false, files, rec.resume.into_iter().collect::<Vec<_>>())
            }
        };
        delete_files(&to_delete, &claims);
        inner.notify_change();
        // Give a plain (UI) thread the courtesy of the file being gone on
        // return; never block inside the runtime.
        if running && tokio::runtime::Handle::try_current().is_err() {
            inner.wait_until(REMOVE_WAIT, |st| !st.running.contains_key(&id));
        }
    }

    /// Discard progress and download again from scratch.
    ///
    /// The partial file is deleted; an already completed file is kept (the
    /// new download gets a unique name next to it).
    pub fn redownload(&self, id: u64) {
        let changed = {
            let mut guard = self.inner.state.lock();
            let st = &mut *guard;
            st.retries.remove(&id);
            if st.request_stop(id, StopReason::Restart) {
                true
            } else if let Some(idx) = st.records.iter().position(|r| r.id == id) {
                let in_use = paths_in_use(st, id);
                let rec = &mut st.records[idx];
                let parts = owned_files(rec, &in_use, false);
                let claims: Vec<ResumeData> = rec.resume.iter().cloned().collect();
                reset_progress(rec);
                delete_files(&parts, &claims);
                st.mark_dirty(SAVE_SOON);
                self.inner.start_locked(st, id, false);
                true
            } else {
                false
            }
        };
        if changed {
            self.inner.notify_change();
        }
    }

    /// Pause every active download and stop the queue.
    pub fn pause_all(&self) {
        {
            let mut guard = self.inner.state.lock();
            let st = &mut *guard;
            st.queue_running = false;
            let ids: Vec<u64> = st.running.keys().copied().collect();
            for id in ids {
                st.request_stop(id, StopReason::Pause);
            }
        }
        self.inner.notify_change();
    }

    /// Start every paused or failed download: queue members through the
    /// (then running) queue, everything else immediately.
    pub fn resume_all(&self) {
        {
            let mut guard = self.inner.state.lock();
            let st = &mut *guard;
            let ids: Vec<u64> = st
                .records
                .iter()
                .filter(|r| !r.queued && matches!(r.status, Status::Paused | Status::Failed))
                .map(|r| r.id)
                .collect();
            for id in ids {
                st.retries.remove(&id);
                self.inner.start_locked(st, id, false);
            }
            self.inner.start_queue_locked(st);
        }
        self.inner.notify_change();
    }

    pub fn set_queued(&self, id: u64, queued: bool) {
        {
            let mut guard = self.inner.state.lock();
            let st = &mut *guard;
            let Some(rec) = st.records.iter_mut().find(|r| r.id == id) else { return };
            if rec.queued == queued || rec.status == Status::Completed {
                return;
            }
            rec.queued = queued;
            if queued && matches!(rec.status, Status::Paused | Status::Failed) {
                rec.status = Status::Queued;
            } else if !queued && rec.status == Status::Queued {
                rec.status = Status::Paused;
            }
            if !queued && let Some(run) = st.running.get_mut(&id) {
                run.by_queue = false;
            }
            st.retries.remove(&id);
            st.mark_dirty(SAVE_SOON);
            self.inner.pump_queue(st);
        }
        self.inner.notify_change();
    }

    pub fn start_queue(&self) {
        self.inner.start_queue_locked(&mut self.inner.state.lock());
        self.inner.notify_change();
    }

    pub fn stop_queue(&self) {
        self.inner.state.lock().stop_queue();
        self.inner.notify_change();
    }

    pub fn queue_running(&self) -> bool {
        self.inner.state.lock().queue_running
    }

    /// All downloads, oldest first.
    pub fn snapshot(&self) -> Vec<ItemView> {
        let st = self.inner.state.lock();
        st.records.iter().map(|r| view(r, st.running.get(&r.id))).collect()
    }

    pub fn item(&self, id: u64) -> Option<ItemView> {
        let st = self.inner.state.lock();
        st.record(id).map(|r| view(r, st.running.get(&id)))
    }

    /// Live segment map of an active download (empty otherwise).
    pub fn segments(&self, id: u64) -> Vec<SegmentView> {
        let st = self.inner.state.lock();
        st.running.get(&id).map(|r| r.progress.segments()).unwrap_or_default()
    }

    /// Sum of the speeds of all active downloads, bytes/s.
    pub fn total_speed(&self) -> f64 {
        self.inner.state.lock().running.values().map(|r| r.speed).sum()
    }

    pub fn settings(&self) -> Settings {
        self.inner.state.lock().settings.clone()
    }

    /// Apply and persist settings (speed limit takes effect immediately).
    pub fn update_settings(&self, settings: Settings) {
        let inner = &self.inner;
        let settings = settings.normalized();
        let minute = clock::local_minutes();
        {
            let mut guard = inner.state.lock();
            let st = &mut *guard;
            inner.global_limiter.set_rate(settings.speed_limit_kib.saturating_mul(1024));
            let schedule_changed = st.settings.schedule != settings.schedule;
            st.settings = settings.clone();
            if schedule_changed && schedule_window_open(&st.settings.schedule, minute) {
                inner.start_queue_locked(st);
            } else {
                inner.pump_queue(st);
            }
        }
        {
            let _guard = inner.save_lock.lock();
            if let Err(e) = settings.save(&inner.data_dir) {
                tracing::warn!(error = %e, "failed to save settings");
            }
        }
        inner.notify_change();
    }

    /// Pause everything and persist synchronously. Call before exiting.
    ///
    /// Waits up to ~5 s for running downloads to write their final resume
    /// checkpoints. Downloads that were active are saved as such, so the
    /// next launch resumes them if `resume_on_launch` is set.
    pub fn shutdown(&self) {
        let inner = &self.inner;
        {
            let mut guard = inner.state.lock();
            let st = &mut *guard;
            st.shutting_down = true;
            let ids: Vec<u64> = st.running.keys().copied().collect();
            for id in ids {
                st.request_stop(id, StopReason::Shutdown);
            }
        }
        inner.stop_ticker.cancel();

        let wait = || inner.wait_until(SHUTDOWN_WAIT, |st| st.running.is_empty());
        match tokio::runtime::Handle::try_current() {
            Err(_) => wait(),
            // The download tasks cannot make progress while this thread
            // blocks; save what the last checkpoints hold instead.
            Ok(h) if h.runtime_flavor() == RuntimeFlavor::CurrentThread => {}
            Ok(_) => tokio::task::block_in_place(wait),
        }

        {
            // Downloads that did not stop in time keep their latest durable
            // checkpoint.
            let mut guard = inner.state.lock();
            let st = &mut *guard;
            for (id, run) in &st.running {
                if let Some(rec) = st.records.iter_mut().find(|r| r.id == *id)
                    && let Some(cp) = run.progress.checkpoint()
                {
                    rec.downloaded = cp.written();
                    rec.resume = Some(cp);
                }
            }
        }
        inner.save_now();
        inner.notify_change();
    }

    /// Change the per-download speed limit (bytes/s; `None` or 0 =
    /// unlimited). Takes effect immediately if the download is running.
    pub fn set_speed_limit(&self, id: u64, limit: Option<u64>) {
        let limit = limit.filter(|&l| l > 0);
        {
            let mut guard = self.inner.state.lock();
            let st = &mut *guard;
            let Some(rec) = st.records.iter_mut().find(|r| r.id == id) else { return };
            rec.speed_limit = limit;
            if let Some(run) = st.running.get(&id) {
                run.limiter.set_rate(limit.unwrap_or(0));
            }
            st.mark_dirty(SAVE_SOON);
        }
        self.inner.notify_change();
    }

    /// The directory holding `downloads.json` and `settings.json`.
    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }
}

impl Inner {
    /// Spawn the engine for record `id`. Returns `false` if it cannot start
    /// (unknown, already running, completed, or shutting down).
    fn start_locked(self: &Arc<Self>, st: &mut State, id: u64, by_queue: bool) -> bool {
        if st.shutting_down || st.running.contains_key(&id) {
            return false;
        }
        let Some(rec) = st.records.iter_mut().find(|r| r.id == id) else { return false };
        if rec.status == Status::Completed {
            return false;
        }
        let s = &st.settings;
        let limiter = Arc::new(RateLimiter::new(rec.speed_limit.unwrap_or(0)));
        let opts = EngineOptions {
            connections: rec.connections.clamp(1, 32),
            min_split: s.min_split_kib.max(1).saturating_mul(1024),
            max_retries: s.max_retries,
            net: s.net.clone(),
            limiters: vec![self.global_limiter.clone(), limiter.clone()],
            expected_sha256: rec.sha256.clone(),
            ..EngineOptions::default()
        };
        let req = DownloadRequest {
            url: rec.url.clone(),
            headers: rec.headers.clone(),
            file_name: requested_name(rec),
            dir: rec.dir.clone(),
        };
        let resume = rec.resume.clone();
        let kind = rec.kind;
        rec.status = Status::Connecting;
        rec.error = None;
        rec.finished = None;

        st.next_gen += 1;
        let generation = st.next_gen;
        let progress = Arc::new(Progress::default());
        let cancel = CancellationToken::new();
        st.running.insert(
            id,
            Running {
                generation,
                progress: progress.clone(),
                cancel: cancel.clone(),
                limiter,
                by_queue,
                stop: None,
                remove_files: Vec::new(),
            remove_claim: None,
                speed: 0.0,
                last_received: 0,
                last_sample: Instant::now(),
                baseline: false,
            },
        );
        st.mark_dirty(SAVE_SOON);

        let engine = self.rt.spawn(async move {
            match kind {
                DownloadKind::Http => download::run(req, opts, resume, progress, cancel).await,
                DownloadKind::Hls => crate::hls::run(req, opts, progress, cancel).await,
            }
        });
        let inner = self.clone();
        self.rt.spawn(async move {
            // A panicking engine must not leave the record "running" forever.
            let result = engine.await.unwrap_or_else(|e| Err(Error::Other(format!("download task failed: {e}"))));
            inner.finish(id, generation, result);
        });
        true
    }

    /// Apply the outcome of an engine run.
    fn finish(self: &Arc<Self>, id: u64, generation: u64, result: Result<Outcome>) {
        let mut to_delete = Vec::new();
        let mut claims: Vec<ResumeData> = Vec::new();
        let mut completed = None;
        let shutting_down;
        {
            let mut guard = self.state.lock();
            let st = &mut *guard;
            if st.running.get(&id).is_none_or(|r| r.generation != generation) {
                return;
            }
            let Some(run) = st.running.remove(&id) else { return };
            self.task_ended.notify_all();
            shutting_down = st.shutting_down;
            let checkpoint = run.progress.checkpoint();
            let info = run.progress.info();
            let received = run.progress.received();
            let in_use = if run.stop == Some(StopReason::Restart) { paths_in_use(st, id) } else { HashSet::new() };

            if let Some(StopReason::Remove { delete_file }) = run.stop {
                if delete_file {
                    to_delete = run.remove_files;
                    claims.extend(run.remove_claim);
                    claims.extend(checkpoint.iter().cloned());
                    to_delete.extend(checkpoint.map(|c| c.part_path()));
                    to_delete.extend(info.map(|i| naming::with_suffix(&i.file_path, PART_SUFFIX)));
                    if let Ok(Outcome::Completed { path, .. }) = result {
                        to_delete.push(path);
                    }
                }
            } else if let Some(rec) = st.records.iter_mut().find(|r| r.id == id) {
                if let Some(info) = &info {
                    apply_info(rec, info);
                }
                if run.stop == Some(StopReason::Restart) {
                    let mut parts = owned_files(rec, &in_use, false);
                    let claims: Vec<ResumeData> = rec.resume.iter().cloned().chain(checkpoint.iter().cloned()).collect();
                    parts.extend(checkpoint.map(|c| c.part_path()));
                    reset_progress(rec);
                    delete_files(&parts, &claims);
                    st.retries.remove(&id);
                    self.start_locked(st, id, false);
                } else {
                    match result {
                        Ok(Outcome::Completed { path, size }) => {
                            if let Some(name) = path.file_name() {
                                rec.file_name = name.to_string_lossy().into_owned();
                                rec.category = Category::for_file_name(&rec.file_name);
                            }
                            rec.file_path = Some(path.clone());
                            rec.total = Some(size);
                            rec.downloaded = size;
                            rec.status = Status::Completed;
                            rec.finished = Some(unix_now());
                            rec.resume = None;
                            rec.error = None;
                            rec.queued = false;
                            st.retries.remove(&id);
                            completed = Some(path);
                        }
                        other => {
                            let resume = checkpoint.or_else(|| rec.resume.take());
                            // Without range support the next run starts over.
                            let resumable = info.as_ref().is_none_or(|i| i.resumable);
                            rec.downloaded =
                                if resumable { resume.as_ref().map_or(received, ResumeData::written) } else { 0 };
                            rec.resume = resume;
                            match (run.stop, other) {
                                (Some(StopReason::Shutdown), _) => {}
                                (Some(StopReason::Requeue), _) => {
                                    rec.status = if rec.queued { Status::Queued } else { Status::Paused };
                                }
                                (Some(_), _) | (None, Ok(_)) => rec.status = Status::Paused,
                                (None, Err(e)) => {
                                    rec.error = Some(e.to_string());
                                    let attempts = st.retries.get(&id).map_or(0, |r| r.attempts);
                                    let retry = run.by_queue
                                        && rec.queued
                                        && st.queue_running
                                        && !st.shutting_down
                                        && attempts < QUEUE_RETRIES
                                        && retryable(&e);
                                    if retry {
                                        rec.status = Status::Queued;
                                        let delay = QUEUE_RETRY_BASE * 2u32.pow(attempts);
                                        st.retries
                                            .insert(id, Retry { attempts: attempts + 1, not_before: Instant::now() + delay });
                                    } else {
                                        rec.status = Status::Failed;
                                        st.retries.remove(&id);
                                    }
                                }
                            }
                        }
                    }
                    if run.stop == Some(StopReason::Continue) {
                        self.start_locked(st, id, false);
                    }
                }
            }
            st.mark_dirty(SAVE_SOON);
            self.pump_queue(st);
        }

        delete_files(&to_delete, &claims);
        if let Some(path) = completed {
            let callback = self.on_complete.lock().clone();
            if let Some(f) = callback {
                f(id, path);
            }
        }
        if shutting_down {
            // The ticker is gone; persist late results directly.
            self.save_now();
        }
        self.notify_change();
    }

    /// Start waiting queue members while slots are free. Returns whether
    /// anything was started.
    fn pump_queue(self: &Arc<Self>, st: &mut State) -> bool {
        if !st.queue_running || st.shutting_down {
            return false;
        }
        let limit = st.settings.max_parallel.max(1);
        let now = Instant::now();
        let mut started = false;
        while st.running.len() < limit {
            let next = st
                .records
                .iter()
                .find(|r| {
                    r.queued
                        && r.status == Status::Queued
                        && !st.running.contains_key(&r.id)
                        && st.retries.get(&r.id).is_none_or(|x| x.not_before <= now)
                })
                .map(|r| r.id);
            let Some(id) = next else { break };
            if !self.start_locked(st, id, true) {
                break;
            }
            started = true;
        }
        started
    }

    fn start_queue_locked(self: &Arc<Self>, st: &mut State) {
        st.queue_running = true;
        for rec in &mut st.records {
            if rec.queued && matches!(rec.status, Status::Paused | Status::Failed) && !st.running.contains_key(&rec.id) {
                rec.status = Status::Queued;
            }
        }
        st.retries.clear();
        st.mark_dirty(SAVE_SOON);
        self.pump_queue(st);
    }

    /// One scheduler step: sample speeds, mirror progress, run the schedule
    /// and the queue.
    fn tick(self: &Arc<Self>) -> TickResult {
        let minute = clock::local_minutes();
        let mut guard = self.state.lock();
        let st = &mut *guard;
        if st.shutting_down {
            return TickResult::default();
        }
        let now = Instant::now();
        let mut changed = false;
        let mut checkpointed = false;
        for (id, run) in st.running.iter_mut() {
            if let Some(rec) = st.records.iter_mut().find(|r| r.id == *id) {
                let (c, cp) = sync_running(rec, run, now);
                changed |= c;
                checkpointed |= cp;
            }
        }
        if checkpointed {
            st.mark_dirty(SAVE_PROGRESS);
        }

        if let Some(prev) = st.sched_prev
            && st.settings.schedule.enabled
        {
            let s = st.settings.schedule.clone();
            let start_hit = crossed(prev, minute, minute_of(s.start));
            let stop_hit = s.stop.is_some_and(|t| crossed(prev, minute, minute_of(t)));
            if start_hit || stop_hit {
                // If both were passed in one step (e.g. after standby), the
                // window decides which one is current.
                let open = s.stop.is_none() || schedule_window_open(&s, minute);
                if start_hit && open {
                    if !st.queue_running {
                        tracing::info!("schedule: starting the queue");
                    }
                    self.start_queue_locked(st);
                    changed = true;
                } else if stop_hit && !open {
                    if st.queue_running {
                        tracing::info!("schedule: stopping the queue");
                    }
                    st.stop_queue();
                    changed = true;
                }
            }
        }
        st.sched_prev = Some(minute);

        changed |= self.pump_queue(st);
        TickResult { save: st.save_due.is_some_and(|d| d <= now), notify: changed || !st.running.is_empty() }
    }

    /// Write `downloads.json` (and the id counter) atomically.
    fn save_now(&self) {
        let _guard = self.save_lock.lock();
        let (records, meta) = {
            let mut st = self.state.lock();
            st.save_due = None;
            (serde_json::to_vec_pretty(&st.records), Meta { next_id: st.next_id })
        };
        let result = records
            .map_err(std::io::Error::from)
            .and_then(|json| paths::write_atomic(&self.data_dir.join(DOWNLOADS_FILE), &json))
            .and_then(|()| {
                let json = serde_json::to_vec(&meta)?;
                paths::write_atomic(&self.data_dir.join(META_FILE), &json)
            });
        if let Err(e) = result {
            tracing::warn!(error = %e, "failed to save the download list");
            self.state.lock().mark_dirty(SAVE_RETRY);
        }
    }

    fn notify_change(&self) {
        let callback = self.on_change.lock().clone();
        if let Some(f) = callback {
            f();
        }
    }

    /// Block the calling thread until `done` holds or `timeout` passes.
    fn wait_until(&self, timeout: Duration, mut done: impl FnMut(&State) -> bool) {
        let deadline = Instant::now() + timeout;
        let mut st = self.state.lock();
        while !done(&st) {
            if self.task_ended.wait_until(&mut st, deadline).timed_out() {
                break;
            }
        }
    }
}

async fn ticker(inner: Weak<Inner>, stop: CancellationToken) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = interval.tick() => {}
        }
        let Some(inner) = inner.upgrade() else { break };
        let result = inner.tick();
        if result.save {
            let saver = inner.clone();
            let _ = tokio::task::spawn_blocking(move || saver.save_now()).await;
        }
        if result.notify {
            inner.notify_change();
        }
    }
}

/// Mirror a running download's live progress into its record. Returns
/// `(visible change, new checkpoint)`.
fn sync_running(rec: &mut DownloadRecord, run: &mut Running, now: Instant) -> (bool, bool) {
    let progress = &run.progress;
    let phase = progress.phase();
    let received = progress.received();

    // Speed: exponentially smoothed byte deltas. While connecting (and on
    // the first sample) only the baseline is taken, so that the bytes already
    // on disk when resuming are not counted as transferred.
    let dt = now.duration_since(run.last_sample).as_secs_f64();
    if phase == Phase::Connecting || !run.baseline {
        run.baseline = phase != Phase::Connecting;
        run.last_received = received;
        run.last_sample = now;
        if phase == Phase::Connecting {
            run.speed = 0.0;
        }
    } else if dt > 0.05 {
        if received < run.last_received {
            // The engine started over (the file changed on the server).
            run.speed = 0.0;
        } else {
            let inst = (received - run.last_received) as f64 / dt;
            let alpha = 1.0 - (-dt / SPEED_TAU_SECS).exp();
            run.speed = if run.speed == 0.0 { inst } else { run.speed + alpha * (inst - run.speed) };
        }
        run.last_received = received;
        run.last_sample = now;
    }

    let mut changed = false;
    if rec.status.is_active() {
        let status = phase_status(phase);
        if rec.status != status {
            rec.status = status;
            changed = true;
        }
    }
    if let Some(info) = progress.info() {
        changed |= apply_info(rec, &info);
    }
    if rec.downloaded != received {
        rec.downloaded = received;
        changed = true;
    }
    let mut checkpointed = false;
    if let Some(cp) = progress.checkpoint()
        && rec.resume.as_ref() != Some(&cp)
    {
        rec.resume = Some(cp);
        checkpointed = true;
    }
    (changed, checkpointed)
}

/// Copy what the first response revealed into the record.
fn apply_info(rec: &mut DownloadRecord, info: &FileInfo) -> bool {
    let mut changed = false;
    if !info.file_name.is_empty() && rec.file_name != info.file_name {
        rec.file_name = info.file_name.clone();
        rec.category = Category::for_file_name(&rec.file_name);
        changed = true;
    }
    if rec.file_path.as_ref() != Some(&info.file_path) {
        rec.file_path = Some(info.file_path.clone());
        changed = true;
    }
    if info.total.is_some() && rec.total != info.total {
        rec.total = info.total;
        changed = true;
    }
    changed
}

fn view(rec: &DownloadRecord, run: Option<&Running>) -> ItemView {
    let mut v = ItemView {
        id: rec.id,
        url: rec.url.clone(),
        kind: rec.kind,
        file_name: rec.file_name.clone(),
        dir: rec.dir.clone(),
        file_path: rec.file_path.clone(),
        category: rec.category,
        status: rec.status,
        total: rec.total,
        downloaded: rec.downloaded,
        speed: 0.0,
        eta: None,
        active_connections: 0,
        connections: rec.connections,
        resumable: rec.resume.as_ref().map(|r| r.resumable),
        error: rec.error.clone(),
        added: rec.added,
        finished: rec.finished,
        queued: rec.queued,
    };
    if let Some(run) = run {
        let p = &run.progress;
        v.downloaded = p.received();
        v.speed = run.speed;
        v.active_connections = p.active_connections();
        if rec.status.is_active() {
            v.status = phase_status(p.phase());
        }
        if let Some(info) = p.info() {
            v.resumable = Some(info.resumable);
            v.total = info.total.or(v.total);
            if !info.file_name.is_empty() {
                v.category = Category::for_file_name(&info.file_name);
                v.file_name = info.file_name;
            }
            v.file_path = Some(info.file_path);
        }
        if let Some(total) = v.total
            && v.speed > 1.0
        {
            v.eta = Some(Duration::from_secs_f64(total.saturating_sub(v.downloaded) as f64 / v.speed));
        }
    }
    v
}

fn phase_status(phase: Phase) -> Status {
    match phase {
        Phase::Connecting => Status::Connecting,
        Phase::Downloading => Status::Downloading,
        Phase::Finishing | Phase::Verifying => Status::Finishing,
    }
}

/// Failures worth another automatic attempt from the queue.
fn retryable(e: &Error) -> bool {
    e.is_transient() || matches!(e, Error::Other(_) | Error::RemoteChanged)
}

/// Forget all progress so the next start downloads from scratch.
fn reset_progress(rec: &mut DownloadRecord) {
    rec.resume = None;
    rec.downloaded = 0;
    rec.total = None;
    rec.file_path = None;
    rec.finished = None;
    rec.error = None;
    rec.status = Status::Paused;
}

/// Partial-download files that may exist for `rec` (never the final file).
fn part_files(rec: &DownloadRecord) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(r) = &rec.resume {
        files.push(r.part_path());
    }
    if let Some(p) = &rec.file_path {
        files.push(naming::with_suffix(p, PART_SUFFIX));
    }
    files.dedup();
    files
}

/// The sidecar `spate-cli get` keeps next to a partial file.
const SIDECAR_SUFFIX: &str = ".json";

/// The files on disk that belong to `rec`: its partial file, with the
/// sidecar of an interrupted `spate-cli get` of the same address if there is
/// one, and with `with_final` the file of a completed download. Paths that
/// another download also uses (`in_use`) are never included, nor a partial
/// file whose sidecar describes another address.
fn owned_files(rec: &DownloadRecord, in_use: &HashSet<PathBuf>, with_final: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for part in part_files(rec) {
        let sidecar = naming::with_suffix(&part, SIDECAR_SUFFIX);
        match sidecar_is_for(&sidecar, rec) {
            Some(true) => files.extend([part, sidecar]),
            Some(false) => {}
            None => files.push(part),
        }
    }
    if with_final
        && rec.status == Status::Completed
        && let Some(p) = &rec.file_path
    {
        files.push(p.clone());
    }
    files.retain(|p| !in_use.contains(p));
    files
}

/// Whether the sidecar at `path` describes `rec`'s download; `None` if there
/// is none or it cannot be read.
fn sidecar_is_for(path: &Path, rec: &DownloadRecord) -> Option<bool> {
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let theirs = [json["url"].as_str(), json["resume"]["final_url"].as_str()];
    let ours = [Some(rec.url.as_str()), rec.resume.as_ref().map(|r| r.final_url.as_str())];
    Some(theirs.iter().flatten().any(|t| ours.iter().flatten().any(|o| o.trim() == t.trim())))
}

/// Every path the downloads other than `id` point at, including what their
/// running engines are writing.
fn paths_in_use(st: &State, id: u64) -> HashSet<PathBuf> {
    let mut used = HashSet::new();
    for rec in st.records.iter().filter(|r| r.id != id) {
        used.extend(part_files(rec));
        used.extend(rec.file_path.iter().cloned());
    }
    for run in st.running.iter().filter(|(other, _)| **other != id).map(|(_, run)| run) {
        if let Some(info) = run.progress.info() {
            used.insert(naming::with_suffix(&info.file_path, PART_SUFFIX));
            used.insert(info.file_path);
        }
        used.extend(run.progress.checkpoint().map(|c| c.part_path()));
    }
    used
}

/// Delete `paths`. A partial file that one of `claims` (the download's resume
/// points) marks with an ownership token is deleted only while it still
/// carries that token, so a file another download has since created at the
/// same path is left alone.
fn delete_files(paths: &[PathBuf], claims: &[ResumeData]) {
    for path in paths {
        let mut claimed = claims.iter().filter(|c| c.part_token.is_some() && c.part_path() == *path).peekable();
        if claimed.peek().is_some() {
            if !claimed.any(download::discard_part) {
                tracing::debug!(path = %path.display(), "partial file is gone or not this download's; left alone");
            }
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "failed to delete file"),
        }
    }
}

/// The name a download gets before the server says otherwise.
fn default_name(url: &str, kind: DownloadKind) -> String {
    let name = Url::parse(url).ok().and_then(|u| naming::from_url(&u)).unwrap_or_else(|| "download".to_string());
    match kind {
        DownloadKind::Http => name,
        DownloadKind::Hls => hls_name(&name),
    }
}

/// The name to force on the engine: one that was chosen, even if it equals
/// the URL-derived default (the name confirmed in a dialog must be the name
/// saved), or one an earlier run already settled on. Otherwise the server's
/// `Content-Disposition` name wins.
fn requested_name(rec: &DownloadRecord) -> Option<String> {
    (rec.name_chosen || rec.file_name != default_name(&rec.url, rec.kind)).then(|| rec.file_name.clone())
}

/// Request headers that carry credentials are never saved (the same list
/// that keeps them on their own origin).
pub use crate::http::is_credential_header;

fn serialize_without_credentials<S: serde::Serializer>(
    headers: &[(String, String)],
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    s.collect_seq(headers.iter().filter(|(name, _)| !is_credential_header(name)))
}

/// HLS streams are saved as MPEG-TS.
fn hls_name(name: &str) -> String {
    let (stem, ext) = naming::split_ext(name);
    if ext.is_empty() || ext.eq_ignore_ascii_case("m3u8") || ext.eq_ignore_ascii_case("m3u") {
        format!("{stem}.ts")
    } else {
        name.to_string()
    }
}

fn detect_kind(url: &str) -> DownloadKind {
    match Url::parse(url) {
        Ok(u) if is_hls_url(&u) => DownloadKind::Hls,
        _ => DownloadKind::Http,
    }
}

fn is_hls_url(url: &Url) -> bool {
    url.path().to_ascii_lowercase().ends_with(".m3u8")
}

fn is_hls_mime(mime: &str) -> bool {
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "application/vnd.apple.mpegurl" | "application/x-mpegurl" | "audio/mpegurl" | "audio/x-mpegurl"
    )
}

/// One `Range: bytes=0-0` request: name, size, range support and type.
async fn probe_url(url: &str, headers: &[(String, String)], net: &NetConfig) -> Result<ProbeInfo> {
    let parsed = Url::parse(url.trim()).map_err(|e| Error::InvalidUrl(e.to_string()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::InvalidUrl(format!("unsupported scheme `{}`", parsed.scheme())));
    }
    let client = http::build_client(net)?;
    let mut extra = header::HeaderMap::new();
    extra.insert(header::RANGE, header::HeaderValue::from_static("bytes=0-0"));
    let resp =
        http::get(&client, &parsed, &parsed, &http::header_map(headers), extra, net.max_redirects, |_| Ok(())).await?;
    let h = resp.headers();
    let (total, resumable) = match resp.status().as_u16() {
        206 => {
            let total = header_str(h, header::CONTENT_RANGE).and_then(http::parse_content_range).and_then(|(_, _, t)| t);
            (total, total.is_some())
        }
        200 => {
            let ranges = header_str(h, header::ACCEPT_RANGES).is_some_and(|v| v.eq_ignore_ascii_case("bytes"));
            (resp.content_length(), ranges && resp.content_length().is_some())
        }
        code => return Err(Error::Status(code)),
    };
    let final_url = resp.url().clone();
    let content_type = header_str(h, header::CONTENT_TYPE).map(str::to_string);
    let mut name = header_str(h, header::CONTENT_DISPOSITION)
        .and_then(naming::from_content_disposition)
        .or_else(|| naming::from_url(&final_url))
        .or_else(|| naming::from_url(&parsed))
        .unwrap_or_else(|| "download".to_string());
    let hls = content_type.as_deref().is_some_and(is_hls_mime) || is_hls_url(&final_url) || is_hls_url(&parsed);
    if hls {
        name = hls_name(&name);
    } else if naming::split_ext(&name).1.is_empty()
        && let Some(ext) = content_type.as_deref().and_then(naming::extension_for_mime)
    {
        name = format!("{name}.{ext}");
    }
    Ok(ProbeInfo {
        file_name: name,
        // The size of a playlist says nothing about the stream.
        total: if hls { None } else { total },
        resumable: resumable && !hls,
        content_type,
        final_url: final_url.to_string(),
        kind: if hls { DownloadKind::Hls } else { DownloadKind::Http },
    })
}

fn load_records(data_dir: &Path) -> Vec<DownloadRecord> {
    let path = data_dir.join(DOWNLOADS_FILE);
    let parsed = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Vec<DownloadRecord>>(&bytes).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => Err(e.to_string()),
    };
    parsed.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "download list unreadable; moved to {DOWNLOADS_BACKUP}, starting empty");
        let backup = data_dir.join(DOWNLOADS_BACKUP);
        if std::fs::rename(&path, &backup).is_err() {
            let _ = std::fs::copy(&path, &backup);
        }
        Vec::new()
    })
}

fn load_meta(data_dir: &Path) -> Meta {
    std::fs::read(data_dir.join(META_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

const MINUTES_PER_DAY: u32 = 24 * 60;

fn minute_of((hour, minute): (u8, u8)) -> u32 {
    u32::from(hour.min(23)) * 60 + u32::from(minute.min(59))
}

/// Whether the local clock moved forward past `target` going from minute
/// `prev` to minute `now` (wrapping at midnight). Backward jumps (DST, clock
/// corrections) and absurd gaps never count.
fn crossed(prev: u32, now: u32, target: u32) -> bool {
    if prev == now {
        return false;
    }
    let span = (now + MINUTES_PER_DAY - prev) % MINUTES_PER_DAY;
    let dist = (target + MINUTES_PER_DAY - prev) % MINUTES_PER_DAY;
    span <= MINUTES_PER_DAY / 2 && dist != 0 && dist <= span
}

/// Whether `now` lies inside the schedule's daily start..stop window.
fn schedule_window_open(s: &Schedule, now: u32) -> bool {
    let Some(stop) = s.stop.filter(|_| s.enabled) else { return false };
    let (start, stop) = (minute_of(s.start), minute_of(stop));
    if start <= stop { start <= now && now < stop } else { now >= start || now < stop }
}

/// Local wall-clock time without a date/time dependency.
mod clock {
    /// Minutes since local midnight.
    pub fn local_minutes() -> u32 {
        imp()
    }

    #[cfg(windows)]
    fn imp() -> u32 {
        use windows_sys::Win32::Foundation::SYSTEMTIME;
        use windows_sys::Win32::System::SystemInformation::GetLocalTime;
        // SAFETY: GetLocalTime only writes the SYSTEMTIME it is given, which
        // is a plain-old-data struct for which all-zero is a valid value.
        let st = unsafe {
            let mut st = std::mem::zeroed::<SYSTEMTIME>();
            GetLocalTime(&mut st);
            st
        };
        u32::from(st.wHour) * 60 + u32::from(st.wMinute)
    }

    #[cfg(unix)]
    fn imp() -> u32 {
        // SAFETY: `time` with a null pointer only returns the time;
        // `localtime_r` writes only into the `tm` it is given (all-zero is a
        // valid `tm`).
        unsafe {
            let t = libc::time(std::ptr::null_mut());
            let mut tm = std::mem::zeroed::<libc::tm>();
            if libc::localtime_r(&t, &mut tm).is_null() {
                return utc_minutes();
            }
            (tm.tm_hour.clamp(0, 23) * 60 + tm.tm_min.clamp(0, 59)) as u32
        }
    }

    #[cfg(not(any(windows, unix)))]
    fn imp() -> u32 {
        utc_minutes()
    }

    #[cfg(not(windows))]
    fn utc_minutes() -> u32 {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        ((secs / 60) % (24 * 60)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const KIB: usize = 1024;
    const MIB: usize = 1024 * 1024;
    const TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn a_claimed_partial_file_is_deleted_only_while_it_carries_the_token() {
        let dir = std::env::temp_dir().join(format!("spate-claims-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let token = [7u8; 16];
        let claim = ResumeData {
            final_url: "https://example.com/a.bin".into(),
            file_path: dir.join("a.bin"),
            total: Some(4),
            resumable: true,
            etag: None,
            last_modified: None,
            segments: Vec::new(),
            part_token: Some(token.iter().map(|b| format!("{b:02x}")).collect()),
        };
        let part = claim.part_path();

        // Another download has since created a partial file at the same path.
        std::fs::write(&part, b"someone else's bytes").unwrap();
        delete_files(std::slice::from_ref(&part), std::slice::from_ref(&claim));
        assert!(part.exists(), "another download's partial file was deleted");

        // Our own file (it ends with the token) goes.
        std::fs::write(&part, [&b"data"[..], &token].concat()).unwrap();
        delete_files(std::slice::from_ref(&part), std::slice::from_ref(&claim));
        assert!(!part.exists());

        // Files no resume point claims are deleted by path, as before.
        let other = dir.join("b.bin.part");
        std::fs::write(&other, b"x").unwrap();
        delete_files(std::slice::from_ref(&other), std::slice::from_ref(&claim));
        assert!(!other.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- minimal HTTP/1.1 range server -------------------------------------

    struct Server {
        base: String,
        len: u64,
        sha: String,
    }

    fn payload(len: usize) -> Vec<u8> {
        let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ len as u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    /// Serve `len` deterministic bytes at every path, honouring `Range`.
    /// `rate` throttles each connection to that many bytes/s. Paths under
    /// `/cd/` come with `Content-Disposition: attachment; filename="invoice.exe"`.
    async fn serve(len: usize, rate: Option<usize>) -> Server {
        let data = Arc::new(payload(len));
        let sha = hex::encode(Sha256::digest(&data[..]));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = data.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(handle(sock, shared.clone(), rate));
            }
        });
        Server { base: format!("http://{addr}"), len: len as u64, sha }
    }

    async fn handle(mut sock: TcpStream, data: Arc<Vec<u8>>, rate: Option<usize>) -> std::io::Result<()> {
        let mut head = Vec::new();
        let mut buf = [0u8; 2048];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut buf).await?;
            if n == 0 || head.len() > 64 * KIB {
                return Ok(());
            }
            head.extend_from_slice(&buf[..n]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let len = data.len() as u64;
        let range = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim().eq_ignore_ascii_case("range").then(|| v.trim().to_string())
            })
            .and_then(|v| parse_range(&v, len));
        let (start, end) = range.unwrap_or((0, len));
        let mut resp = format!(
            "HTTP/1.1 {}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\n\
             Content-Type: application/octet-stream\r\nConnection: close\r\n",
            if range.is_some() { "206 Partial Content" } else { "200 OK" },
            end - start
        );
        if range.is_some() {
            resp += &format!("Content-Range: bytes {start}-{}/{len}\r\n", end - 1);
        }
        if head.split_whitespace().nth(1).is_some_and(|path| path.starts_with("/cd/")) {
            resp += "Content-Disposition: attachment; filename=\"invoice.exe\"\r\n";
        }
        resp += "\r\n";
        sock.write_all(resp.as_bytes()).await?;
        let (mut pos, end) = (start as usize, end as usize);
        while pos < end {
            let n = (16 * KIB).min(end - pos);
            sock.write_all(&data[pos..pos + n]).await?;
            pos += n;
            if let Some(rate) = rate {
                tokio::time::sleep(Duration::from_secs_f64(n as f64 / rate as f64)).await;
            }
        }
        sock.shutdown().await
    }

    fn parse_range(v: &str, len: u64) -> Option<(u64, u64)> {
        let (a, b) = v.strip_prefix("bytes=")?.split_once('-')?;
        let start: u64 = a.trim().parse().ok()?;
        let end = match b.trim() {
            "" => len,
            b => b.parse::<u64>().ok()?.saturating_add(1).min(len),
        };
        (start < end).then_some((start, end))
    }

    // ---- harness -------------------------------------------------------------

    struct Env {
        data: PathBuf,
        dl: PathBuf,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.data);
        }
    }

    /// A fresh data dir whose settings keep downloads inside it.
    fn env(name: &str, tweak: impl FnOnce(&mut Settings)) -> Env {
        let data = std::env::temp_dir().join(format!("spate-mgr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        let dl = data.join("dl");
        let mut s = Settings {
            download_dir: dl.clone(),
            category_folders: false,
            start_queue_on_launch: false,
            ..Settings::default()
        };
        tweak(&mut s);
        s.save(&data).unwrap();
        Env { data, dl }
    }

    fn manager(env: &Env) -> Manager {
        Manager::new(tokio::runtime::Handle::current(), env.data.clone()).unwrap()
    }

    fn request(url: String, start: StartMode) -> AddRequest {
        AddRequest { url, start, ..Default::default() }
    }

    async fn wait_for(m: &Manager, id: u64, what: &str, pred: impl Fn(&ItemView) -> bool) -> ItemView {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let v = m.item(id).unwrap_or_else(|| panic!("item {id} vanished while waiting for {what}"));
            if pred(&v) {
                return v;
            }
            assert!(v.status != Status::Failed, "failed while waiting for {what}: {v:?}");
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {v:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_cond(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn record(m: &Manager, id: u64) -> DownloadRecord {
        m.inner.state.lock().record(id).cloned().unwrap()
    }

    fn saved_records(env: &Env) -> Vec<DownloadRecord> {
        serde_json::from_slice(&std::fs::read(env.data.join(DOWNLOADS_FILE)).unwrap()).unwrap()
    }

    // ---- tests ------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn add_downloads_to_completion() {
        let env = env("complete", |_| {});
        let srv = serve(3 * MIB + 123, None).await;
        let m = manager(&env);
        let completions = Arc::new(Mutex::new(Vec::new()));
        let c = completions.clone();
        m.set_on_complete(move |id, path| c.lock().push((id, path)));
        let changes = Arc::new(AtomicUsize::new(0));
        let c = changes.clone();
        m.set_on_change(move || {
            c.fetch_add(1, Ordering::Relaxed);
        });

        let url = format!("{}/files/data.bin", srv.base);
        let probe = m.probe(url.clone(), vec![]).await.unwrap().unwrap();
        assert_eq!(probe.file_name, "data.bin");
        assert_eq!(probe.total, Some(srv.len));
        assert!(probe.resumable);
        assert_eq!(probe.kind, DownloadKind::Http);

        let id = m.add(request(url, StartMode::Now));
        let v = wait_for(&m, id, "completion", |v| v.status == Status::Completed).await;
        let path = v.file_path.clone().unwrap();
        assert_eq!(path, env.dl.join("data.bin"));
        assert_eq!(download::sha256_file(&path).unwrap(), srv.sha);
        assert_eq!((v.downloaded, v.total), (srv.len, Some(srv.len)));
        assert!(v.finished.is_some() && v.error.is_none() && !v.queued);
        assert!(m.segments(id).is_empty());
        wait_cond("on_complete", || !completions.lock().is_empty()).await;
        assert_eq!(*completions.lock(), vec![(id, path)]);
        assert!(changes.load(Ordering::Relaxed) > 0);

        m.shutdown();
        let saved = saved_records(&env);
        assert_eq!((saved.len(), saved[0].status), (1, Status::Completed));
        assert!(saved[0].resume.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pause_and_resume_complete_with_correct_bytes() {
        let env = env("pause", |_| {});
        let srv = serve(4 * MIB, Some(MIB)).await;
        let m = manager(&env);
        let id = m.add(AddRequest { connections: Some(4), ..request(format!("{}/p.bin", srv.base), StartMode::Now) });

        wait_for(&m, id, "progress", |v| v.downloaded >= 256 * KIB as u64).await;
        assert!(!m.segments(id).is_empty());
        m.pause(id);
        let v = wait_for(&m, id, "pause", |v| v.status == Status::Paused).await;
        assert!(v.downloaded > 0 && v.downloaded < srv.len, "{v:?}");
        assert_eq!((v.speed, v.active_connections), (0.0, 0));
        let resume = record(&m, id).resume.expect("checkpoint stored on pause");
        assert_eq!(resume.written(), v.downloaded);
        assert!(resume.part_path().exists());

        m.resume(id);
        let before = v.downloaded;
        wait_for(&m, id, "more progress", |v| v.downloaded >= before + 128 * KIB as u64).await;
        // Resume pressed while the pause is still winding down is not lost.
        m.pause(id);
        m.resume(id);
        let v = wait_for(&m, id, "completion", |v| v.status == Status::Completed).await;
        assert_eq!(download::sha256_file(v.file_path.as_ref().unwrap()).unwrap(), srv.sha);
        assert!(!resume.part_path().exists());
        m.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn list_survives_restart_and_resumes() {
        let env = env("persist", |_| {});
        let srv = serve(4 * MIB, Some(MIB)).await;
        let (active, parked) = {
            let m = manager(&env);
            let active = m.add(AddRequest { connections: Some(2), ..request(format!("{}/a.bin", srv.base), StartMode::Now) });
            let parked = m.add(request(format!("{}/b.bin", srv.base), StartMode::Paused));
            wait_for(&m, active, "progress", |v| v.downloaded >= 256 * KIB as u64).await;
            // While downloading, from a plain thread like the GUI's.
            let gui = m.clone();
            tokio::task::spawn_blocking(move || std::thread::spawn(move || gui.shutdown()).join().unwrap())
                .await
                .unwrap();
            assert!(m.inner.state.lock().running.is_empty(), "shutdown waits for the final checkpoints");
            (active, parked)
        };
        let saved = saved_records(&env);
        assert!(saved[0].status.is_active(), "an interrupted download is saved as active");
        assert!(saved[0].resume.is_some());

        let m = manager(&env);
        let a = m.item(active).unwrap();
        assert_eq!(a.status, Status::Paused);
        assert!(a.downloaded > 0 && a.downloaded < srv.len);
        let b = m.item(parked).unwrap();
        assert_eq!((b.status, b.downloaded, b.file_name.as_str()), (Status::Paused, 0, "b.bin"));

        m.resume(active);
        let a = wait_for(&m, active, "completion", |v| v.status == Status::Completed).await;
        assert_eq!(download::sha256_file(a.file_path.as_ref().unwrap()).unwrap(), srv.sha);

        // Ids are never reused, even after the newest record is removed.
        let newest = m.add(request(format!("{}/c.bin", srv.base), StartMode::Paused));
        assert!(newest > parked);
        m.remove(newest, false);
        m.shutdown();
        drop(m);
        let m = manager(&env);
        assert!(m.add(request(format!("{}/d.bin", srv.base), StartMode::Paused)) > newest);
        m.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn queue_respects_max_parallel_and_stop() {
        let env = env("queue", |s| s.max_parallel = 2);
        let srv = serve(512 * KIB, Some(MIB)).await;
        let m = manager(&env);
        assert!(!m.queue_running());
        let ids: Vec<u64> = (0..4)
            .map(|i| m.add(AddRequest { connections: Some(1), ..request(format!("{}/q{i}.bin", srv.base), StartMode::Queue) }))
            .collect();
        assert!(m.snapshot().iter().all(|v| v.status == Status::Queued && v.queued));

        m.start_queue();
        assert!(m.queue_running());
        let active = || m.snapshot().iter().filter(|v| v.status.is_active()).count();
        assert_eq!(active(), 2);

        // Stopping the queue puts its running downloads back in line.
        m.stop_queue();
        wait_cond("queue stop", || active() == 0).await;
        assert!(m.snapshot().iter().all(|v| v.status == Status::Queued), "{:?}", m.snapshot());

        m.start_queue();
        let mut max_active = 0;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let snap = m.snapshot();
            let n = snap.iter().filter(|v| v.status.is_active()).count();
            assert!(n <= 2, "{n} active: {snap:?}");
            max_active = max_active.max(n);
            assert!(snap.iter().all(|v| v.status != Status::Failed), "{snap:?}");
            if snap.iter().all(|v| v.status == Status::Completed) {
                break;
            }
            assert!(Instant::now() < deadline, "queue did not finish: {snap:?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(max_active, 2);
        for id in ids {
            let v = m.item(id).unwrap();
            assert_eq!(download::sha256_file(v.file_path.as_ref().unwrap()).unwrap(), srv.sha);
        }
        m.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remove_deletes_files_on_request() {
        let env = env("remove", |_| {});
        let small = serve(256 * KIB, None).await;
        let slow = serve(4 * MIB, Some(MIB)).await;
        let m = manager(&env);

        let done = m.add(request(format!("{}/gone.bin", small.base), StartMode::Now));
        let path = wait_for(&m, done, "completion", |v| v.status == Status::Completed).await.file_path.unwrap();
        m.remove(done, true);
        assert!(m.item(done).is_none());
        assert!(!path.exists());

        let kept = m.add(request(format!("{}/kept.bin", small.base), StartMode::Now));
        let path = wait_for(&m, kept, "completion", |v| v.status == Status::Completed).await.file_path.unwrap();
        m.remove(kept, false);
        assert!(m.item(kept).is_none());
        assert!(path.exists());

        // Removing a running download deletes its partial file once stopped.
        let partial = m.add(request(format!("{}/partial.bin", slow.base), StartMode::Now));
        let v = wait_for(&m, partial, "progress", |v| v.downloaded > 0 && v.file_path.is_some()).await;
        let part = naming::with_suffix(v.file_path.as_ref().unwrap(), PART_SUFFIX);
        assert!(part.exists());
        m.remove(partial, true);
        assert!(m.item(partial).is_none());
        wait_cond("partial file deletion", || !part.exists() && m.inner.state.lock().running.is_empty()).await;
        m.shutdown();
        assert!(saved_records(&env).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn corrupt_list_is_backed_up_and_names_are_derived() {
        let env = env("corrupt", |s| s.category_folders = true);
        std::fs::write(env.data.join(DOWNLOADS_FILE), b"{not json").unwrap();
        let m = manager(&env);
        assert!(m.snapshot().is_empty());
        assert_eq!(std::fs::read(env.data.join(DOWNLOADS_BACKUP)).unwrap(), b"{not json");

        let zip = m.item(m.add(request("http://127.0.0.1:9/files/x.zip".into(), StartMode::Paused))).unwrap();
        assert_eq!((zip.file_name.as_str(), zip.category, zip.kind), ("x.zip", Category::Compressed, DownloadKind::Http));
        assert_eq!(zip.dir, env.dl.join("Compressed"));
        let hls = m.item(m.add(request("https://cdn.test/live/index.m3u8?t=1".into(), StartMode::Paused))).unwrap();
        assert_eq!((hls.file_name.as_str(), hls.category, hls.kind), ("index.ts", Category::Video, DownloadKind::Hls));
        let named = m.add(AddRequest { file_name: Some("my:name.iso".into()), ..request("http://h/x".into(), StartMode::Paused) });
        assert_eq!(m.item(named).unwrap().file_name, "my_name.iso");
        assert_eq!(requested_name(&record(&m, named)).as_deref(), Some("my_name.iso"));
        assert_eq!(requested_name(&record(&m, zip.id)), None);
        let same = m.add(AddRequest { file_name: Some("x.zip".into()), ..request("http://h/files/x.zip".into(), StartMode::Paused) });
        assert_eq!(requested_name(&record(&m, same)).as_deref(), Some("x.zip"), "chosen, though equal to the URL's");
        m.remove(same, false);

        // Unusable URLs fail with a readable error instead of hanging.
        let bad = m.add(request("ftp://h/f.zip".into(), StartMode::Now));
        wait_cond("failure", || m.item(bad).is_some_and(|v| v.status == Status::Failed)).await;
        assert!(m.item(bad).unwrap().error.unwrap().contains("ftp"));
        m.remove(bad, true);

        let queued = m.add(request("http://h/q.zip".into(), StartMode::Queue));
        m.pause(queued);
        assert_eq!(m.item(queued).unwrap().status, Status::Paused);
        m.set_queued(queued, true); // already a member: unchanged
        m.set_queued(zip.id, true);
        assert_eq!(m.item(zip.id).unwrap().status, Status::Queued);
        m.set_queued(zip.id, false);
        assert_eq!(m.item(zip.id).unwrap().status, Status::Paused);
        m.shutdown();
        assert_eq!(saved_records(&env).len(), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn confirmed_names_win_over_the_server() {
        let env = env("names", |_| {});
        let srv = serve(64 * KIB, None).await;
        let m = manager(&env);
        // The confirmed name equals the one in the URL; the server suggests
        // another. What was confirmed is what gets saved.
        let url = format!("{}/cd/invoice.pdf", srv.base);
        let confirmed = m.add(AddRequest { file_name: Some("invoice.pdf".into()), ..request(url.clone(), StartMode::Now) });
        let v = wait_for(&m, confirmed, "completion", |v| v.status == Status::Completed).await;
        assert_eq!(v.file_path.unwrap(), env.dl.join("invoice.pdf"));
        assert_eq!(requested_name(&record(&m, confirmed)).as_deref(), Some("invoice.pdf"));
        // Without a chosen name the server's name is used, as before.
        let unnamed = m.add(request(url, StartMode::Now));
        let v = wait_for(&m, unnamed, "completion", |v| v.status == Status::Completed).await;
        assert_eq!(v.file_path.unwrap(), env.dl.join("invoice.exe"));
        m.shutdown();
        assert!(saved_records(&env).iter().any(|r| r.id == confirmed && r.name_chosen));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn credentials_are_never_saved() {
        let env = env("creds", |_| {});
        let headers: Vec<(String, String)> = [
            ("Cookie", "session=SECRET1"),
            ("Authorization", "Bearer SECRET2"),
            ("X-Auth-Token", "SECRET3"),
            ("Proxy-Authorization", "Basic SECRET4"),
            ("Referer", "https://site.example/"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let m = manager(&env);
        let id = m.add(AddRequest { headers: headers.clone(), ..request("http://127.0.0.1:9/f.zip".into(), StartMode::Paused) });
        assert_eq!(record(&m, id).headers, headers, "kept in memory for this session");
        m.shutdown();
        let json = String::from_utf8(std::fs::read(env.data.join(DOWNLOADS_FILE)).unwrap()).unwrap();
        assert!(!json.contains("SECRET"), "{json}");
        assert!(json.contains("https://site.example/"));
        drop(m);
        // After a restart the download goes on without them.
        let referer = vec![("Referer".to_string(), "https://site.example/".to_string())];
        let m = manager(&env);
        assert_eq!(record(&m, id).headers, referer);
        m.shutdown();
        drop(m);

        // A list written by an older version is cleaned up on load.
        let mut old: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        old[0]["headers"] = serde_json::json!([["Cookie", "session=SECRET1"], ["Referer", "https://site.example/"]]);
        std::fs::write(env.data.join(DOWNLOADS_FILE), serde_json::to_vec(&old).unwrap()).unwrap();
        let m = manager(&env);
        assert_eq!(record(&m, id).headers, referer);
        m.shutdown();
        let json = String::from_utf8(std::fs::read(env.data.join(DOWNLOADS_FILE)).unwrap()).unwrap();
        assert!(!json.contains("SECRET"), "{json}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deleting_files_takes_only_the_downloads_own() {
        let env = env("ownfiles", |_| {});
        std::fs::create_dir_all(&env.dl).unwrap();
        let m = manager(&env);
        let file = |name: &str| env.dl.join(name);
        let touch = |name: &str, body: &str| std::fs::write(file(name), body).unwrap();
        let failed_at = |url: &str, name: &str| {
            let id = m.add(request(url.into(), StartMode::Paused));
            let mut st = m.inner.state.lock();
            let rec = st.record_mut(id).unwrap();
            rec.file_path = Some(file(name));
            rec.status = Status::Failed;
            id
        };

        // A failed download's leftover partial file and its sidecar go.
        let a = failed_at("http://127.0.0.1:9/a.bin", "a.bin");
        touch("a.bin.part", "partial");
        touch("a.bin.part.json", r#"{"spate_sidecar":1,"url":"http://127.0.0.1:9/a.bin","resume":{}}"#);
        m.remove(a, true);
        assert!(!file("a.bin.part").exists() && !file("a.bin.part.json").exists());

        // A partial file whose sidecar describes another download stays.
        let b = failed_at("http://127.0.0.1:9/b.bin", "b.bin");
        touch("b.bin.part", "someone else's");
        touch("b.bin.part.json", r#"{"spate_sidecar":1,"url":"https://other.example/b.bin","resume":{}}"#);
        m.remove(b, true);
        assert!(file("b.bin.part").exists() && file("b.bin.part.json").exists());

        // A path another download in the list also points at stays.
        let c = failed_at("http://127.0.0.1:9/c.bin", "c.bin");
        let d = failed_at("http://127.0.0.1:9/c.bin?again", "c.bin");
        touch("c.bin.part", "shared");
        m.remove(c, true);
        assert!(file("c.bin.part").exists());
        m.remove(d, true);
        assert!(!file("c.bin.part").exists());

        // The final name of a download that never completed is not proof
        // that the file there is its own.
        let e = failed_at("http://127.0.0.1:9/e.bin", "e.bin");
        touch("e.bin", "not ours");
        m.remove(e, true);
        assert!(file("e.bin").exists());
        m.shutdown();
    }

    #[test]
    fn credential_headers() {
        for name in ["Cookie", "cookie2", "Authorization", "Proxy-Authorization", "X-Auth-Token", "X-CSRF-Token", "X-Api-Key"] {
            assert!(is_credential_header(name), "{name}");
        }
        for name in ["Referer", "User-Agent", "Accept", "Accept-Language", "Origin", "Range"] {
            assert!(!is_credential_header(name), "{name}");
        }
    }

    #[test]
    fn manager_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<Manager>();
    }

    #[test]
    fn schedule_edges_and_window() {
        // Forward across the target, including over midnight.
        assert!(crossed(599, 600, 600));
        assert!(!crossed(600, 600, 600));
        assert!(!crossed(600, 601, 600));
        assert!(crossed(1439, 0, 0));
        assert!(crossed(1430, 5, 2));
        // Clock set back an hour (DST): no events.
        assert!(!crossed(180, 120, 150));

        let s = Schedule { enabled: true, start: (22, 0), stop: Some((6, 30)) };
        assert!(schedule_window_open(&s, minute_of((23, 0))));
        assert!(schedule_window_open(&s, minute_of((6, 29))));
        assert!(!schedule_window_open(&s, minute_of((6, 30))));
        assert!(!schedule_window_open(&s, minute_of((12, 0))));
        let day = Schedule { enabled: true, start: (9, 0), stop: Some((17, 0)) };
        assert!(schedule_window_open(&day, minute_of((9, 0))));
        assert!(!schedule_window_open(&day, minute_of((17, 0))));
        assert!(!schedule_window_open(&Schedule { enabled: false, ..day }, minute_of((10, 0))));
        assert!(!schedule_window_open(&Schedule { stop: None, ..day }, minute_of((10, 0))));
        assert!(clock::local_minutes() < MINUTES_PER_DAY);
    }
}
