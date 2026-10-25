//! The main window: menus, toolbar, category sidebar, download table,
//! details panel with the segment map, status bar and notifications.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Align2, Color32, CornerRadius, Key, KeyboardShortcut, Layout, Modifiers, RichText, Sense, Stroke, vec2};
use egui_extras::{Column, TableBuilder};
use spate_core::ipc::{AddItem, Request};
use spate_core::{Category, ItemView, Manager, Settings, StartMode, Status};
use serde::{Deserialize, Serialize};

use crate::clipwatch::ClipWatcher;
use crate::dialogs::{
    AboutDialog, AddDialog, BatchDialog, DeleteDialog, DialogResult, OptionsDialog, OptionsTab, Source, UiOptions, UpdateDialog, UpdateOutcome,
    request_from_item,
};
use crate::icons::{self, Icon};
use crate::theme::{self, ThemeChoice};
use crate::{format, segmap, sys, update, urls};

/// Events delivered to the UI thread from other threads.
pub enum UiEvent {
    Ipc(Request),
    Completed(u64, PathBuf),
    ClipboardUrl(String),
    HostInstalled(Result<String, String>),
    /// The daily check found a newer release.
    UpdateAvailable(update::Release),
    #[allow(dead_code)]
    Tray(TrayAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum TrayAction {
    Show,
    AddUrl,
    PauseAll,
    ResumeAll,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Filter {
    All,
    Category(Category),
    Unfinished,
    Finished,
    Queued,
}

impl Filter {
    fn matches(self, i: &ItemView) -> bool {
        match self {
            Filter::All => true,
            Filter::Category(c) => i.category == c,
            Filter::Unfinished => i.status != Status::Completed,
            Filter::Finished => i.status == Status::Completed,
            Filter::Queued => i.queued || i.status == Status::Queued,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortKey {
    Added,
    Name,
    Size,
    Progress,
    Speed,
    Eta,
    Status,
    Connections,
}

impl SortKey {
    const ALL: [(SortKey, &'static str); 8] = [
        (SortKey::Added, "Date added"),
        (SortKey::Name, "Name"),
        (SortKey::Size, "Size"),
        (SortKey::Progress, "Progress"),
        (SortKey::Speed, "Speed"),
        (SortKey::Eta, "Time left"),
        (SortKey::Status, "Status"),
        (SortKey::Connections, "Connections"),
    ];
}

fn status_rank(s: Status) -> u8 {
    match s {
        Status::Downloading => 0,
        Status::Connecting => 1,
        Status::Finishing => 2,
        Status::Queued => 3,
        Status::Paused => 4,
        Status::Failed => 5,
        Status::Completed => 6,
    }
}

/// Remove the plain Enter presses from `events`; whether one of them was a
/// fresh press rather than key repeat (holding Enter down opens nothing).
fn take_enter(events: &mut Vec<egui::Event>) -> bool {
    let mut fresh = false;
    events.retain(|e| match e {
        egui::Event::Key { key: Key::Enter, pressed: true, repeat, modifiers, .. } if modifiers.matches_logically(Modifiers::NONE) => {
            fresh |= !repeat;
            false
        }
        _ => true,
    });
    fresh
}

/// Drop key-repeat events of Enter and Space, which also click the focused
/// button: holding a key down must not confirm a dialog.
fn drop_held_keys(events: &mut Vec<egui::Event>) {
    events.retain(|e| !matches!(e, egui::Event::Key { key: Key::Enter | Key::Space, repeat: true, .. }));
}

/// The download Enter opens in the list: the focused one if it is selected
/// and the user picked it.
fn enter_target(focus: Option<u64>, selection: &BTreeSet<u64>, by_user: bool) -> Option<u64> {
    focus.filter(|id| by_user && selection.contains(id))
}

fn compare(a: &ItemView, b: &ItemView, key: SortKey) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let o = match key {
        SortKey::Added => a.added.cmp(&b.added),
        SortKey::Name => a.file_name.to_lowercase().cmp(&b.file_name.to_lowercase()),
        SortKey::Size => a.total.cmp(&b.total),
        SortKey::Progress => {
            let f = |i: &ItemView| if i.status == Status::Completed { 1.0 } else { i.fraction().unwrap_or(0.0) };
            f(a).partial_cmp(&f(b)).unwrap_or(Ordering::Equal)
        }
        SortKey::Speed => a.speed.partial_cmp(&b.speed).unwrap_or(Ordering::Equal),
        SortKey::Eta => a.eta.unwrap_or(Duration::MAX).cmp(&b.eta.unwrap_or(Duration::MAX)),
        SortKey::Status => status_rank(a.status).cmp(&status_rank(b.status)),
        SortKey::Connections => a.active_connections.cmp(&b.active_connections),
    };
    o.then(a.id.cmp(&b.id))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WhenDone {
    #[default]
    Nothing,
    Exit,
    Shutdown,
}

/// UI state persisted by eframe between runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub show_sidebar: bool,
    pub show_details: bool,
    pub theme: ThemeChoice,
    pub sort: SortKey,
    pub ascending: bool,
    pub filter: Filter,
    pub minimize_to_tray: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            show_sidebar: true,
            show_details: true,
            theme: ThemeChoice::System,
            sort: SortKey::Added,
            ascending: true,
            filter: Filter::All,
            minimize_to_tray: false,
        }
    }
}

const PREFS_KEY: &str = "spate.prefs";

enum ToastKind {
    Done { path: PathBuf },
    Failed,
    Info,
    Shutdown,
}

struct Toast {
    id: u64,
    kind: ToastKind,
    title: String,
    body: String,
    created: Instant,
    ttl: Duration,
}

#[derive(Debug)]
enum Action {
    Click { id: u64, ctrl: bool, shift: bool },
    DoubleClick(u64),
    ContextSelect(u64),
    Open(u64),
    OpenWith(u64),
    Reveal(u64),
    CopyUrl(u64),
    Pause(Vec<u64>),
    Resume(Vec<u64>),
    Redownload(Vec<u64>),
    Queue(Vec<u64>, bool),
    Delete(Vec<u64>, bool),
    Details(u64),
    Sort(SortKey),
    /// Per-download speed limit in bytes/s (`None` = unlimited).
    Limit(Vec<u64>, Option<u64>),
}

pub struct App {
    m: Manager,
    rx: Receiver<UiEvent>,
    tx: Sender<UiEvent>,
    items: Vec<ItemView>,
    settings: Settings,
    prefs: Prefs,
    search: String,
    focus_search: bool,
    selection: BTreeSet<u64>,
    anchor: Option<u64>,
    focus_item: Option<u64>,
    /// `focus_item` was picked by the user (clicked), not by the app (a
    /// download just added). Only then does Enter open it.
    focus_by_user: bool,
    /// Displayed order of rows (indexes into `items`), rebuilt every frame.
    rows: Vec<usize>,
    add_dialog: Option<AddDialog>,
    captures: VecDeque<AddDialog>,
    capture_total: usize,
    batch: Option<BatchDialog>,
    delete: Option<DeleteDialog>,
    options: Option<OptionsDialog>,
    update: Option<UpdateDialog>,
    /// Set to the new executable once an update is installed; `main` starts
    /// it after this instance has shut down.
    restart: Arc<OnceLock<PathBuf>>,
    about: bool,
    shortcuts: bool,
    toasts: Vec<Toast>,
    next_toast: u64,
    prev_status: HashMap<u64, Status>,
    clip: ClipWatcher,
    when_done: WhenDone,
    was_busy: bool,
    background_start: bool,
    first_frame: bool,
    tray: Option<crate::tray::Tray>,
    hidden_to_tray: bool,
    window_focused: bool,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        m: Manager,
        rx: Receiver<UiEvent>,
        tx: Sender<UiEvent>,
        args: urls::Args,
        restart: Arc<OnceLock<PathBuf>>,
    ) -> Self {
        let ctx = cc.egui_ctx.clone();
        theme::install(&ctx);
        let prefs: Prefs = cc.storage.and_then(|s| eframe::get_value(s, PREFS_KEY)).unwrap_or_default();
        prefs.theme.apply(&ctx);
        let settings = m.settings();

        let clip = ClipWatcher::start(tx.clone(), ctx.clone());
        clip.set_enabled(settings.clipboard_monitor);

        let tray = crate::tray::Tray::new(&ctx, tx.clone());
        update::spawn_daily_check(m.clone(), tx.clone(), ctx.clone());

        let mut app = Self {
            m,
            rx,
            tx,
            items: Vec::new(),
            settings,
            prefs,
            search: String::new(),
            focus_search: false,
            selection: BTreeSet::new(),
            anchor: None,
            focus_item: None,
            focus_by_user: false,
            rows: Vec::new(),
            add_dialog: None,
            captures: VecDeque::new(),
            capture_total: 0,
            batch: None,
            delete: None,
            options: None,
            update: None,
            restart,
            about: false,
            shortcuts: false,
            toasts: Vec::new(),
            next_toast: 0,
            prev_status: HashMap::new(),
            clip,
            when_done: WhenDone::Nothing,
            was_busy: false,
            background_start: args.background,
            first_frame: true,
            tray,
            hidden_to_tray: false,
            window_focused: true,
        };
        if args.updated {
            app.toast(ToastKind::Info, "Spate updated", format!("You now have version {}.", update::CURRENT), 8);
        }
        if !args.urls.is_empty() {
            let items = args.urls.into_iter().map(|url| AddItem { url, ..Default::default() }).collect();
            app.handle_add(&ctx, items, true, StartMode::Now);
        }
        app
    }

    // -----------------------------------------------------------------------
    // Events
    // -----------------------------------------------------------------------

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                UiEvent::Ipc(Request::Show) => self.show_window(ctx),
                UiEvent::Ipc(Request::Ping) => {}
                UiEvent::Ipc(Request::Add { items, interactive, start }) => {
                    self.handle_add(ctx, items, interactive, start.unwrap_or(StartMode::Now))
                }
                UiEvent::Completed(id, path) => self.on_completed(ctx, id, path),
                UiEvent::ClipboardUrl(url) => {
                    if self.add_dialog.is_none() && !self.items.iter().any(|i| i.url == url) {
                        self.add_dialog = Some(AddDialog::new(url, Source::ClipboardWatch, &self.settings));
                        self.announce_dialog(ctx);
                    }
                }
                UiEvent::HostInstalled(r) => {
                    if let Some(o) = &mut self.options {
                        o.host_running = false;
                        o.host_output = Some(r);
                    }
                }
                UiEvent::UpdateAvailable(release) => {
                    // Shown once no other dialog is open (see `dialogs`).
                    let skipped = self.settings.skipped_version.as_deref() == Some(release.version.as_str());
                    if self.update.is_none() && self.settings.check_for_updates && !skipped {
                        self.update = Some(UpdateDialog::offer(release, self.settings.net.clone()));
                    }
                }
                UiEvent::Tray(a) => match a {
                    TrayAction::Show => self.show_window(ctx),
                    TrayAction::AddUrl => {
                        self.show_window(ctx);
                        self.open_add_dialog();
                    }
                    TrayAction::PauseAll => self.m.pause_all(),
                    TrayAction::ResumeAll => self.m.resume_all(),
                    TrayAction::Exit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                },
            }
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        if self.hidden_to_tray {
            self.hidden_to_tray = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Make a dialog that opened by itself noticeable without taking the
    /// keyboard focus from whatever the user is typing into (a web page could
    /// time a download to catch an Enter): the taskbar button flashes, and
    /// the user switches over when they choose to.
    fn announce_dialog(&mut self, ctx: &egui::Context) {
        if self.hidden_to_tray {
            // A hidden window has no taskbar button; show it, unfocused.
            self.hidden_to_tray = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
        if !self.window_focused {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Critical));
        }
    }

    fn handle_add(&mut self, ctx: &egui::Context, items: Vec<AddItem>, interactive: bool, start: StartMode) {
        let items: Vec<AddItem> = items.into_iter().filter(|i| urls::is_http_url(&i.url)).collect();
        if items.is_empty() {
            return;
        }
        if interactive && self.settings.show_capture_dialog {
            if items.len() == 1 && self.batch.is_none() {
                self.captures.push_back(AddDialog::from_capture(&items[0], &self.settings));
                self.capture_total += 1;
            } else if let Some(b) = &mut self.batch {
                b.extend(items);
            } else {
                self.batch = Some(BatchDialog::new(items, false, &self.settings));
            }
            self.announce_dialog(ctx);
        } else {
            let n = items.len();
            for item in &items {
                self.m.add(request_from_item(item, start));
            }
            let body = if n == 1 { urls::file_name_from_url(&items[0].url) } else { format!("{n} downloads") };
            let title = match start {
                StartMode::Now => "Download started",
                StartMode::Queue => "Added to queue",
                StartMode::Paused => "Download added",
            };
            self.toast(ToastKind::Info, title, body, 4);
        }
    }

    fn on_completed(&mut self, ctx: &egui::Context, id: u64, path: PathBuf) {
        if !self.settings.notify_on_complete {
            return;
        }
        let name = self
            .m
            .item(id)
            .map(|i| i.file_name)
            .unwrap_or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let size = self.m.item(id).and_then(|i| i.total).map(format::size).unwrap_or_default();
        let body = if size.is_empty() { name } else { format!("{name} · {size}") };
        self.toast(ToastKind::Done { path }, "Download complete", body, 10);
        if !self.window_focused {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
        }
    }

    fn toast(&mut self, kind: ToastKind, title: &str, body: String, secs: u64) {
        self.next_toast += 1;
        self.toasts.push(Toast { id: self.next_toast, kind, title: title.into(), body, created: Instant::now(), ttl: Duration::from_secs(secs) });
        if self.toasts.len() > 5 {
            self.toasts.remove(0);
        }
    }

    fn open_add_dialog(&mut self) {
        if self.add_dialog.is_some() {
            return;
        }
        let clip = arboard::Clipboard::new().and_then(|mut c| c.get_text()).ok().and_then(|t| urls::single_url(&t));
        self.add_dialog = Some(match clip {
            Some(url) => AddDialog::new(url, Source::Clipboard, &self.settings),
            None => AddDialog::new(String::new(), Source::Typed, &self.settings),
        });
    }

    fn open_batch_from_clipboard(&mut self) {
        let text = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
        let found = urls::extract_urls(&text);
        match found.len() {
            0 => self.toast(ToastKind::Info, "Nothing to add", "The clipboard holds no web addresses.".into(), 4),
            1 => self.add_dialog = Some(AddDialog::new(found.into_iter().next().unwrap_or_default(), Source::Clipboard, &self.settings)),
            _ => self.batch = Some(BatchDialog::from_urls(found, &self.settings)),
        }
    }

    fn apply_settings(&mut self, s: Settings) {
        self.clip.set_enabled(s.clipboard_monitor);
        self.m.update_settings(s.clone());
        self.settings = s;
    }

    fn selected_ids(&self) -> Vec<u64> {
        self.items.iter().filter(|i| self.selection.contains(&i.id)).map(|i| i.id).collect()
    }

    fn item(&self, id: u64) -> Option<&ItemView> {
        self.items.iter().find(|i| i.id == id)
    }

    fn open_item(&mut self, id: u64) {
        let Some(item) = self.item(id).cloned() else { return };
        if item.status == Status::Completed {
            let path = item.file_path.clone().unwrap_or_else(|| item.dir.join(&item.file_name));
            if let Err(e) = sys::open_path(&path) {
                self.toast(ToastKind::Failed, "Could not open the file", e.to_string(), 6);
            }
        } else {
            self.focus_item = Some(id);
            self.prefs.show_details = true;
        }
    }

    fn reveal_item(&mut self, id: u64) {
        let Some(item) = self.item(id).cloned() else { return };
        let path = item.file_path.clone().unwrap_or_else(|| item.dir.join(&item.file_name));
        let target = if path.exists() { path } else { item.dir.clone() };
        if let Err(e) = sys::reveal(&target) {
            self.toast(ToastKind::Failed, "Could not open the folder", e.to_string(), 6);
        }
    }

    fn export_list(&mut self) {
        let Some(path) = rfd::FileDialog::new().set_title("Export download list").set_file_name("spate-downloads.txt").add_filter("Text", &["txt"]).save_file() else {
            return;
        };
        let text: String = self.items.iter().map(|i| format!("{}\n", i.url)).collect();
        match std::fs::write(&path, text) {
            Ok(()) => self.toast(ToastKind::Info, "List exported", format!("{} addresses saved to {}", self.items.len(), path.display()), 5),
            Err(e) => self.toast(ToastKind::Failed, "Export failed", e.to_string(), 6),
        }
    }

    fn import_list(&mut self) {
        let Some(path) = rfd::FileDialog::new().set_title("Import addresses").add_filter("Text", &["txt", "csv", "lst"]).add_filter("All files", &["*"]).pick_file() else {
            return;
        };
        match std::fs::read(&path) {
            Ok(bytes) => {
                let found = urls::extract_urls(&String::from_utf8_lossy(&bytes));
                if found.is_empty() {
                    self.toast(ToastKind::Info, "Nothing to import", "The file contains no web addresses.".into(), 5);
                } else {
                    self.batch = Some(BatchDialog::from_urls(found, &self.settings));
                }
            }
            Err(e) => self.toast(ToastKind::Failed, "Import failed", e.to_string(), 6),
        }
    }

    fn request_delete(&mut self, ids: Vec<u64>) {
        if !ids.is_empty() {
            self.delete = Some(DeleteDialog::new(ids, &self.items, false));
        }
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Click { id, ctrl, shift } => {
                    if shift {
                        let anchor = self.anchor.unwrap_or(id);
                        let pos = |x: u64| self.rows.iter().position(|&r| self.items[r].id == x);
                        if let (Some(a), Some(b)) = (pos(anchor), pos(id)) {
                            let (lo, hi) = (a.min(b), a.max(b));
                            if !ctrl {
                                self.selection.clear();
                            }
                            for r in &self.rows[lo..=hi] {
                                self.selection.insert(self.items[*r].id);
                            }
                        }
                    } else if ctrl {
                        if !self.selection.remove(&id) {
                            self.selection.insert(id);
                        }
                        self.anchor = Some(id);
                    } else {
                        self.selection.clear();
                        self.selection.insert(id);
                        self.anchor = Some(id);
                    }
                    self.focus_item = Some(id);
                    self.focus_by_user = true;
                }
                Action::ContextSelect(id) => {
                    if !self.selection.contains(&id) {
                        self.selection.clear();
                        self.selection.insert(id);
                        self.anchor = Some(id);
                    }
                    self.focus_item = Some(id);
                    self.focus_by_user = true;
                }
                Action::DoubleClick(id) | Action::Open(id) => self.open_item(id),
                Action::OpenWith(id) => {
                    if let Some(p) = self.item(id).and_then(|i| i.file_path.clone()) {
                        let _ = sys::open_with(&p);
                    }
                }
                Action::Reveal(id) => self.reveal_item(id),
                Action::CopyUrl(id) => {
                    if let Some(url) = self.item(id).map(|i| i.url.clone()) {
                        self.clip.note_own_copy(&url);
                        if let Ok(mut c) = arboard::Clipboard::new() {
                            let _ = c.set_text(url);
                        }
                    }
                }
                Action::Pause(ids) => ids.into_iter().for_each(|id| self.m.pause(id)),
                Action::Resume(ids) => ids.into_iter().for_each(|id| self.m.resume(id)),
                Action::Redownload(ids) => ids.into_iter().for_each(|id| self.m.redownload(id)),
                Action::Queue(ids, q) => ids.into_iter().for_each(|id| self.m.set_queued(id, q)),
                Action::Limit(ids, limit) => ids.into_iter().for_each(|id| self.m.set_speed_limit(id, limit)),
                Action::Delete(ids, with_files) => {
                    if with_files {
                        let mut d = DeleteDialog::new(ids, &self.items, false);
                        d.preset_delete_files();
                        self.delete = Some(d);
                    } else {
                        self.request_delete(ids);
                    }
                }
                Action::Details(id) => {
                    self.focus_item = Some(id);
                    self.prefs.show_details = true;
                }
                Action::Sort(key) => {
                    if self.prefs.sort == key {
                        self.prefs.ascending = !self.prefs.ascending;
                    } else {
                        self.prefs.sort = key;
                        self.prefs.ascending = !matches!(key, SortKey::Size | SortKey::Speed | SortKey::Progress);
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Per-frame bookkeeping
    // -----------------------------------------------------------------------

    fn refresh(&mut self, ctx: &egui::Context) {
        self.items = self.m.snapshot();
        let live: BTreeSet<u64> = self.items.iter().map(|i| i.id).collect();
        self.selection.retain(|id| live.contains(id));
        if self.focus_item.is_some_and(|id| !live.contains(&id)) {
            self.focus_item = None;
        }

        // Failure notifications on status transitions.
        let mut failed = Vec::new();
        for i in &self.items {
            let prev = self.prev_status.insert(i.id, i.status);
            if i.status == Status::Failed && prev.is_some_and(|p| p != Status::Failed) {
                failed.push((i.file_name.clone(), i.error.clone().unwrap_or_default()));
            }
        }
        self.prev_status.retain(|id, _| live.contains(id));
        for (name, err) in failed {
            let body = if err.is_empty() { name } else { format!("{name}: {err}") };
            self.toast(ToastKind::Failed, "Download failed", body, 10);
        }

        // "When all downloads finish" action.
        let queue_running = self.m.queue_running();
        let busy = self.items.iter().any(|i| i.status.is_active() || (queue_running && i.status == Status::Queued));
        if self.was_busy && !busy {
            match std::mem::take(&mut self.when_done) {
                WhenDone::Nothing => {}
                WhenDone::Exit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                WhenDone::Shutdown => match sys::schedule_shutdown(60) {
                    Ok(()) => self.toast(ToastKind::Shutdown, "Shutting down in 60 seconds", "All downloads have finished.".into(), 60),
                    Err(e) => self.toast(ToastKind::Failed, "Could not shut down", e, 8),
                },
            }
        }
        self.was_busy = busy;

        // Display order.
        let needle = self.search.trim().to_lowercase();
        let mut rows: Vec<usize> = (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                self.prefs.filter.matches(it)
                    && (needle.is_empty() || it.file_name.to_lowercase().contains(&needle) || it.url.to_lowercase().contains(&needle))
            })
            .collect();
        let (key, asc) = (self.prefs.sort, self.prefs.ascending);
        rows.sort_by(|&a, &b| {
            let o = compare(&self.items[a], &self.items[b], key);
            if asc { o } else { o.reverse() }
        });
        self.rows = rows;

        if let Some(t) = &self.tray {
            let active = self.items.iter().filter(|i| i.status.is_active()).count();
            t.set_tooltip(&if active == 0 {
                "Spate".to_string()
            } else {
                format!("Spate · {active} active · {}", format::speed(self.m.total_speed()))
            });
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let modal_open = self.any_modal();
        let typing = ctx.egui_wants_keyboard_input();
        let cmd = |k: Key| KeyboardShortcut::new(Modifiers::COMMAND, k);
        if !modal_open {
            if ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::N))) {
                self.open_add_dialog();
            }
            if ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::B))) {
                self.batch = Some(BatchDialog::new(Vec::new(), true, &self.settings));
            }
            if ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::F))) {
                self.focus_search = true;
            }
        }
        if modal_open || typing {
            return;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::A))) {
            self.selection = self.rows.iter().map(|&r| self.items[r].id).collect();
        }
        let paste = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
        if paste {
            self.open_batch_from_clipboard();
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete)) {
            let ids = self.selected_ids();
            self.request_delete(ids);
        }
        if ctx.input_mut(|i| take_enter(&mut i.events))
            && let Some(id) = enter_target(self.focus_item, &self.selection, self.focus_by_user)
        {
            self.open_item(id);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Space)) {
            let ids = self.selected_ids();
            let any_active = ids.iter().any(|id| self.item(*id).is_some_and(|i| i.status.is_active()));
            for id in ids {
                if any_active { self.m.pause(id) } else { self.m.resume(id) }
            }
        }
    }

    fn any_modal(&self) -> bool {
        self.add_dialog.is_some()
            || !self.captures.is_empty()
            || self.batch.is_some()
            || self.delete.is_some()
            || self.options.is_some()
            || self.update.is_some()
            || self.about
            || self.shortcuts
    }

    // -----------------------------------------------------------------------
    // Layout
    // -----------------------------------------------------------------------

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let sel = self.selected_ids();
        let has_sel = !sel.is_empty();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Tasks", |ui| {
                if ui.add(egui::Button::new("Add download…").shortcut_text("Ctrl+N")).clicked() {
                    self.open_add_dialog();
                }
                if ui.add(egui::Button::new("Add several downloads…").shortcut_text("Ctrl+B")).clicked() {
                    self.batch = Some(BatchDialog::new(Vec::new(), true, &self.settings));
                }
                if ui.add(egui::Button::new("Add links from clipboard").shortcut_text("Ctrl+V")).clicked() {
                    self.open_batch_from_clipboard();
                }
                ui.separator();
                if ui.button("Import addresses from file…").clicked() {
                    self.import_list();
                }
                if ui.add_enabled(!self.items.is_empty(), egui::Button::new("Export addresses to file…")).clicked() {
                    self.export_list();
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Downloads", |ui| {
                if ui.add_enabled(has_sel, egui::Button::new("Resume").shortcut_text("Space")).clicked() {
                    sel.iter().for_each(|&id| self.m.resume(id));
                }
                if ui.add_enabled(has_sel, egui::Button::new("Pause")).clicked() {
                    sel.iter().for_each(|&id| self.m.pause(id));
                }
                if ui.button("Pause all").clicked() {
                    self.m.pause_all();
                }
                if ui.button("Resume all").clicked() {
                    self.m.resume_all();
                }
                ui.separator();
                if ui.add_enabled(has_sel, egui::Button::new("Download again from scratch")).clicked() {
                    sel.iter().for_each(|&id| self.m.redownload(id));
                }
                if ui.add_enabled(has_sel, egui::Button::new("Delete…").shortcut_text("Del")).clicked() {
                    self.request_delete(sel.clone());
                }
                let done: Vec<u64> = self.items.iter().filter(|i| i.status == Status::Completed).map(|i| i.id).collect();
                if ui.add_enabled(!done.is_empty(), egui::Button::new("Remove completed…")).clicked() {
                    self.delete = Some(DeleteDialog::new(done, &self.items, true));
                }
                ui.separator();
                if ui.add(egui::Button::new("Select all").shortcut_text("Ctrl+A")).clicked() {
                    self.selection = self.rows.iter().map(|&r| self.items[r].id).collect();
                }
                if ui.add(egui::Button::new("Find…").shortcut_text("Ctrl+F")).clicked() {
                    self.focus_search = true;
                }
            });
            ui.menu_button("Queue", |ui| {
                let running = self.m.queue_running();
                if ui.add_enabled(!running, egui::Button::new("Start queue")).clicked() {
                    self.m.start_queue();
                }
                if ui.add_enabled(running, egui::Button::new("Stop queue")).clicked() {
                    self.m.stop_queue();
                }
                ui.separator();
                if ui.add_enabled(has_sel, egui::Button::new("Add selection to queue")).clicked() {
                    sel.iter().for_each(|&id| self.m.set_queued(id, true));
                }
                if ui.add_enabled(has_sel, egui::Button::new("Take selection out of queue")).clicked() {
                    sel.iter().for_each(|&id| self.m.set_queued(id, false));
                }
                ui.separator();
                ui.menu_button("When all downloads finish", |ui| {
                    ui.radio_value(&mut self.when_done, WhenDone::Nothing, "Do nothing");
                    ui.radio_value(&mut self.when_done, WhenDone::Exit, "Exit Spate");
                    ui.radio_value(&mut self.when_done, WhenDone::Shutdown, "Shut down the computer");
                });
                if ui.button("Schedule…").clicked() {
                    self.open_options(OptionsTab::Scheduler);
                }
            });
            ui.menu_button("View", |ui| {
                ui.checkbox(&mut self.prefs.show_sidebar, "Categories");
                ui.checkbox(&mut self.prefs.show_details, "Details panel");
                ui.separator();
                ui.menu_button("Sort by", |ui| {
                    for (k, label) in SortKey::ALL {
                        if ui.radio(self.prefs.sort == k, label).clicked() {
                            self.prefs.sort = k;
                        }
                    }
                    ui.separator();
                    ui.checkbox(&mut self.prefs.ascending, "Ascending");
                });
                ui.menu_button("Theme", |ui| {
                    for t in [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark] {
                        if ui.radio(self.prefs.theme == t, t.label()).clicked() {
                            self.prefs.theme = t;
                            t.apply(ui.ctx());
                        }
                    }
                });
            });
            ui.menu_button("Options", |ui| {
                if ui.button("Options…").clicked() {
                    self.open_options(OptionsTab::General);
                }
                if ui.button("Browser integration…").clicked() {
                    self.open_options(OptionsTab::Browser);
                }
                ui.separator();
                let mut clip = self.settings.clipboard_monitor;
                if ui.checkbox(&mut clip, "Watch clipboard for links").changed() {
                    let mut s = self.settings.clone();
                    s.clipboard_monitor = clip;
                    self.apply_settings(s);
                }
                let mut ask = self.settings.show_capture_dialog;
                if ui.checkbox(&mut ask, "Confirm browser downloads").changed() {
                    let mut s = self.settings.clone();
                    s.show_capture_dialog = ask;
                    self.apply_settings(s);
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Keyboard shortcuts").clicked() {
                    self.shortcuts = true;
                }
                if ui.button("Open data folder").clicked() {
                    let _ = sys::open_path(&spate_core::paths::data_dir());
                }
                ui.separator();
                if ui.add_enabled(self.update.is_none(), egui::Button::new("Check for updates…")).clicked() {
                    self.update = Some(UpdateDialog::check(self.settings.net.clone(), ui.ctx()));
                }
                if ui.button("About Spate").clicked() {
                    self.about = true;
                }
            });
        });
    }

    fn open_options(&mut self, tab: OptionsTab) {
        let ui_opts = UiOptions { theme: self.prefs.theme, minimize_to_tray: self.prefs.minimize_to_tray, autostart: sys::autostart_enabled() };
        self.options = Some(OptionsDialog::new(tab, &self.settings, ui_opts, self.tray.is_some()));
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let sel = self.selected_ids();
        let any_resumable = sel.iter().any(|id| self.item(*id).is_some_and(|i| !i.status.is_active() && i.status != Status::Completed));
        let any_active = sel.iter().any(|id| self.item(*id).is_some_and(|i| i.status.is_active() || i.status == Status::Queued));
        let running = self.m.queue_running();
        let has_done = self.items.iter().any(|i| i.status == Status::Completed);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if tool_button(ui, Icon::Add, "Add URL", true, "Add a download (Ctrl+N)").clicked() {
                self.open_add_dialog();
            }
            if tool_button(ui, Icon::Batch, "Batch", true, "Add several downloads at once (Ctrl+B)").clicked() {
                self.batch = Some(BatchDialog::new(Vec::new(), true, &self.settings));
            }
            toolbar_gap(ui);
            if tool_button(ui, Icon::Resume, "Resume", any_resumable, "Start or resume the selected downloads").clicked() {
                sel.iter().for_each(|&id| self.m.resume(id));
            }
            if tool_button(ui, Icon::Pause, "Pause", any_active, "Pause the selected downloads").clicked() {
                sel.iter().for_each(|&id| self.m.pause(id));
            }
            if tool_button(ui, Icon::StopAll, "Stop all", true, "Pause every running download").clicked() {
                self.m.pause_all();
            }
            toolbar_gap(ui);
            if tool_button(ui, Icon::Delete, "Delete", !sel.is_empty(), "Remove the selected downloads (Del)").clicked() {
                self.request_delete(sel.clone());
            }
            if tool_button(ui, Icon::DeleteDone, "Clean up", has_done, "Remove all completed downloads from the list").clicked() {
                let done: Vec<u64> = self.items.iter().filter(|i| i.status == Status::Completed).map(|i| i.id).collect();
                self.delete = Some(DeleteDialog::new(done, &self.items, true));
            }
            toolbar_gap(ui);
            let (icon, label, tip) =
                if running { (Icon::QueueStop, "Stop queue", "Stop starting queued downloads") } else { (Icon::QueueStart, "Start queue", "Start the queued downloads") };
            if tool_button(ui, icon, label, true, tip).clicked() {
                if running { self.m.stop_queue() } else { self.m.start_queue() }
            }
            if tool_button(ui, Icon::Options, "Options", true, "Connections, folders, browser integration and more").clicked() {
                self.open_options(OptionsTab::General);
            }

            let room = vec2(ui.available_width(), 56.0);
            ui.allocate_ui_with_layout(room, Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                ui.add_space(4.0);
                self.speed_limit_control(ui);
                speed_readout(ui, self.m.total_speed());
                if ui.available_width() > 120.0 {
                    let w = ui.available_width().min(220.0) - 10.0;
                    let r = ui.add_sized([w, 26.0], egui::TextEdit::singleline(&mut self.search).hint_text("Search…"));
                    if self.focus_search {
                        r.request_focus();
                        self.focus_search = false;
                    }
                    if r.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                        self.search.clear();
                        r.surrender_focus();
                    }
                }
            });
        });
    }

    fn speed_limit_control(&mut self, ui: &mut egui::Ui) {
        const PRESETS: [u64; 9] = [0, 128, 256, 512, 1024, 2048, 5120, 10240, 25600];
        let current = self.settings.speed_limit_kib;
        let mut chosen = current;
        egui::ComboBox::from_id_salt("speed_limit")
            .selected_text(format!("Limit: {}", format::limit_kib(current)))
            .width(150.0)
            .show_ui(ui, |ui| {
                for p in PRESETS {
                    ui.selectable_value(&mut chosen, p, format::limit_kib(p));
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Custom");
                    ui.add(egui::DragValue::new(&mut chosen).range(0..=10_000_000).speed(16).suffix(" KB/s"));
                });
            });
        if chosen != current {
            let mut s = self.settings.clone();
            s.speed_limit_kib = chosen;
            self.apply_settings(s);
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let mut counts: HashMap<Category, usize> = HashMap::new();
        let (mut unfinished, mut finished, mut queued) = (0, 0, 0);
        for i in &self.items {
            *counts.entry(i.category).or_default() += 1;
            if i.status == Status::Completed {
                finished += 1;
            } else {
                unfinished += 1;
            }
            if Filter::Queued.matches(i) {
                queued += 1;
            }
        }
        ui.add_space(8.0);
        ui.label(RichText::new("LIBRARY").small().strong().color(ui.visuals().weak_text_color()));
        sidebar_entry(ui, &mut self.prefs.filter, Filter::All, "All downloads", self.items.len(), None);
        for c in Category::ALL {
            sidebar_entry(ui, &mut self.prefs.filter, Filter::Category(c), c.label(), counts.get(&c).copied().unwrap_or(0), Some(c));
        }
        ui.add_space(10.0);
        ui.label(RichText::new("STATE").small().strong().color(ui.visuals().weak_text_color()));
        sidebar_entry(ui, &mut self.prefs.filter, Filter::Unfinished, "Unfinished", unfinished, None);
        sidebar_entry(ui, &mut self.prefs.filter, Filter::Finished, "Finished", finished, None);
        sidebar_entry(ui, &mut self.prefs.filter, Filter::Queued, "Queued", queued, None);

        ui.add_space(14.0);
        let running = self.m.queue_running();
        egui::Frame::new().fill(ui.visuals().faint_bg_color).corner_radius(8).inner_margin(10).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                icons::dot(ui.painter(), r.center(), 4.5, if running { theme::GOOD } else { theme::MUTED });
                ui.label(RichText::new(if running { "Queue running" } else { "Queue stopped" }).strong());
            });
            let s = &self.settings.schedule;
            if s.enabled {
                let stop = s.stop.map(|(h, m)| format!(" – {h:02}:{m:02}")).unwrap_or_default();
                ui.weak(format!("Daily at {:02}:{:02}{stop}", s.start.0, s.start.1));
            } else {
                ui.weak(format!("{} at a time", self.settings.max_parallel));
            }
        });
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        let mut actions = Vec::new();
        if self.items.is_empty() {
            empty_state(ui, &mut actions);
            if !actions.is_empty() {
                self.open_add_dialog();
            }
            return;
        }
        if self.rows.is_empty() {
            ui.centered_and_justified(|ui| ui.weak("No downloads match this view."));
            return;
        }
        let row_h = 28.0;
        let now = format::now_unix();
        let (sort, asc) = (self.prefs.sort, self.prefs.ascending);
        let modifiers = ui.input(|i| i.modifiers);
        let items = &self.items;
        let rows = &self.rows;
        let selection = &self.selection;
        let header = |ui: &mut egui::Ui, key: SortKey, label: &str, actions: &mut Vec<Action>| {
            let text = RichText::new(label).strong().small();
            let r = ui.add(egui::Label::new(text).sense(Sense::click()).selectable(false));
            if sort == key {
                // Painted sort arrow (the default font has no triangle glyphs).
                let (a, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                let c = ui.visuals().text_color();
                let (top, bottom) = (a.top() + 2.0, a.bottom() - 2.0);
                let pts = if asc {
                    vec![egui::pos2(a.left() + 1.0, bottom), egui::pos2(a.right() - 1.0, bottom), egui::pos2(a.center().x, top)]
                } else {
                    vec![egui::pos2(a.left() + 1.0, top), egui::pos2(a.right() - 1.0, top), egui::pos2(a.center().x, bottom)]
                };
                ui.painter().add(egui::Shape::convex_polygon(pts, c, Stroke::NONE));
            }
            if r.clicked() {
                actions.push(Action::Sort(key));
            }
            r.on_hover_cursor(egui::CursorIcon::PointingHand);
        };

        TableBuilder::new(ui)
            .id_salt("downloads_table")
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(140.0).clip(true))
            .column(Column::initial(78.0).at_least(56.0))
            .column(Column::initial(124.0).at_least(70.0))
            .column(Column::initial(86.0).at_least(56.0))
            .column(Column::initial(72.0).at_least(50.0))
            .column(Column::initial(96.0).at_least(60.0))
            .column(Column::initial(50.0).at_least(40.0))
            .column(Column::initial(96.0).at_least(70.0).clip(true))
            .header(26.0, |mut h| {
                h.col(|ui| header(ui, SortKey::Name, "Name", &mut actions));
                h.col(|ui| header(ui, SortKey::Size, "Size", &mut actions));
                h.col(|ui| header(ui, SortKey::Progress, "Progress", &mut actions));
                h.col(|ui| header(ui, SortKey::Speed, "Speed", &mut actions));
                h.col(|ui| header(ui, SortKey::Eta, "Time left", &mut actions));
                h.col(|ui| header(ui, SortKey::Status, "Status", &mut actions));
                h.col(|ui| header(ui, SortKey::Connections, "Conn.", &mut actions));
                h.col(|ui| header(ui, SortKey::Added, "Added", &mut actions));
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let it = &items[rows[row.index()]];
                    row.set_selected(selection.contains(&it.id));
                    row.col(|ui| {
                        icons::category_glyph(ui, it.category, 18.0);
                        if it.queued {
                            ui.label(RichText::new("Q").small().strong().color(theme::WARN)).on_hover_text("In the queue");
                        }
                        // Shortened in the middle, so the extension stays in view.
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        let name = crate::dialogs::fit_name(ui, &it.file_name, &font, ui.available_width());
                        let r = ui.add(egui::Label::new(name.as_str()).wrap_mode(egui::TextWrapMode::Truncate).selectable(false));
                        if name != it.file_name {
                            r.on_hover_text(&it.file_name);
                        }
                    });
                    row.col(|ui| {
                        ui.label(format::opt_size(it.total));
                    });
                    row.col(|ui| segmap::progress_cell(ui, it));
                    row.col(|ui| {
                        if it.status.is_active() {
                            ui.label(format::speed(it.speed));
                        }
                    });
                    row.col(|ui| {
                        if it.status.is_active() {
                            ui.label(it.eta.map(format::eta).unwrap_or_default());
                        }
                    });
                    row.col(|ui| {
                        let r = ui.colored_label(theme::status_color(it.status, ui), it.status.label());
                        if let Some(e) = &it.error {
                            r.on_hover_text(e);
                        }
                    });
                    row.col(|ui| {
                        if it.status.is_active() {
                            ui.label(format!("{}/{}", it.active_connections, it.connections));
                        } else {
                            ui.weak(format!("{}", it.connections));
                        }
                    });
                    row.col(|ui| {
                        ui.weak(format::short_date(it.added, now)).on_hover_text(format::date_time(it.added));
                    });

                    let resp = row.response();
                    if resp.double_clicked() {
                        actions.push(Action::DoubleClick(it.id));
                    } else if resp.clicked() {
                        actions.push(Action::Click { id: it.id, ctrl: modifiers.command, shift: modifiers.shift });
                    }
                    if resp.secondary_clicked() {
                        actions.push(Action::ContextSelect(it.id));
                    }
                    resp.context_menu(|ui| {
                        let targets: Vec<u64> = if selection.contains(&it.id) {
                            items.iter().filter(|i| selection.contains(&i.id)).map(|i| i.id).collect()
                        } else {
                            vec![it.id]
                        };
                        context_menu(ui, it, targets, &mut actions);
                    });
                });
            });
        self.apply(actions);
    }

    fn details(&mut self, ui: &mut egui::Ui) {
        let Some(it) = self.focus_item.and_then(|id| self.item(id)).cloned() else {
            ui.centered_and_justified(|ui| ui.weak("Select a download to see its details."));
            return;
        };
        let segs = self.m.segments(it.id);
        let mut actions = Vec::new();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                icons::category_glyph(ui, it.category, 28.0);
                ui.vertical(|ui| {
                    ui.add(egui::Label::new(RichText::new(&it.file_name).strong().size(15.0)).truncate());
                    ui.horizontal(|ui| {
                        ui.colored_label(theme::status_color(it.status, ui), it.status.label());
                        ui.weak("·");
                        ui.weak(format::progress_of(it.downloaded, it.total));
                        if let Some(f) = it.fraction() {
                            ui.weak(format!("({})", format::percent(if it.status == Status::Completed { 1.0 } else { f })));
                        }
                        if it.status.is_active() {
                            ui.weak("·");
                            ui.label(format::speed(it.speed));
                            if let Some(e) = it.eta {
                                ui.weak(format!("· {} left", format::eta(e)));
                            }
                        }
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if it.status == Status::Completed {
                        if ui.button("Open").clicked() {
                            actions.push(Action::Open(it.id));
                        }
                    } else if it.status.is_active() || it.status == Status::Queued {
                        if ui.button("Pause").clicked() {
                            actions.push(Action::Pause(vec![it.id]));
                        }
                    } else if ui.button("Resume").clicked() {
                        actions.push(Action::Resume(vec![it.id]));
                    }
                    if ui.button("Show in folder").clicked() {
                        actions.push(Action::Reveal(it.id));
                    }
                });
            });
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label(RichText::new("Segment map").small().strong());
                let active = segs.iter().filter(|s| s.active).count();
                if !segs.is_empty() {
                    ui.weak(format!("{} parts · {} connection(s) receiving", segs.len(), active));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), legend);
            });
            segmap::segment_map(ui, &it, &segs, 26.0);
            ui.add_space(8.0);

            ui.columns(2, |cols| {
                egui::Grid::new("details_grid").num_columns(2).spacing(vec2(12.0, 5.0)).show(&mut cols[0], |ui| {
                    let path = it.file_path.clone().unwrap_or_else(|| it.dir.join(&it.file_name));
                    kv(ui, "Address", |ui| {
                        let r = ui.add(egui::Label::new(RichText::new(&it.url).monospace().small()).truncate().sense(Sense::click()));
                        if r.clone().on_hover_text("Click to copy").clicked() {
                            actions.push(Action::CopyUrl(it.id));
                        }
                    });
                    kv(ui, "Saved to", |ui| {
                        ui.add(egui::Label::new(RichText::new(path.display().to_string()).small()).truncate());
                    });
                    kv(ui, "Size", |ui| {
                        ui.label(format::opt_size(it.total));
                    });
                    if it.resumable.is_some() || it.status != Status::Completed {
                        kv(ui, "Resumable", |ui| {
                            let (t, c) = match it.resumable {
                                Some(true) => ("Yes", theme::GOOD),
                                Some(false) => ("No, a restart begins from zero", theme::WARN),
                                None => ("Not known yet", theme::MUTED),
                            };
                            ui.colored_label(c, t);
                        });
                    }
                    kv(ui, "Kind", |ui| {
                        ui.label(match it.kind {
                            spate_core::DownloadKind::Http => "HTTP file",
                            spate_core::DownloadKind::Hls => "HLS stream (saved as .ts)",
                        });
                    });
                    kv(ui, "Added", |ui| {
                        ui.label(format::date_time(it.added));
                    });
                    if let Some(f) = it.finished {
                        kv(ui, "Finished", |ui| {
                            ui.label(format::date_time(f));
                        });
                    }
                    if let Some(e) = &it.error {
                        kv(ui, "Error", |ui| {
                            ui.add(egui::Label::new(RichText::new(e).color(theme::BAD)).wrap());
                        });
                    }
                });
                let ui = &mut cols[1];
                ui.label(RichText::new(format!("Connections · {} of {} active", it.active_connections, it.connections)).small().strong());
                if segs.is_empty() {
                    ui.weak(if it.status.is_active() { "Waiting for the first response…" } else { "Per-connection details appear while the download runs." });
                } else {
                    segmap::connections_grid(ui, &segs, it.total);
                }
            });
        });
        if it.status.is_active() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        self.apply(actions);
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let active = self.items.iter().filter(|i| i.status.is_active()).count();
        let queued = self.items.iter().filter(|i| i.status == Status::Queued).count();
        ui.horizontal(|ui| {
            ui.weak(format!("{} downloads", self.items.len()));
            ui.weak("·");
            ui.weak(format!("{active} active"));
            ui.weak("·");
            ui.weak(format!("{queued} waiting"));
            if !self.selection.is_empty() {
                ui.weak("·");
                let bytes: u64 = self.items.iter().filter(|i| self.selection.contains(&i.id)).filter_map(|i| i.total).sum();
                ui.weak(format!("{} selected ({})", self.selection.len(), format::size(bytes)));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                match self.when_done {
                    WhenDone::Nothing => {}
                    WhenDone::Exit => {
                        ui.colored_label(theme::WARN, "Exits when done");
                    }
                    WhenDone::Shutdown => {
                        ui.colored_label(theme::WARN, "Shuts down when done");
                    }
                }
                if self.settings.clipboard_monitor {
                    ui.weak("Clipboard watch on");
                }
                ui.weak(format!("Limit {}", format::limit_kib(self.settings.speed_limit_kib)));
            });
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(d) = &mut self.delete {
            match d.show(ctx) {
                DialogResult::Open => {}
                DialogResult::Cancel => self.delete = None,
                DialogResult::Done(files) => {
                    let ids = d.ids.clone();
                    for id in &ids {
                        self.selection.remove(id);
                    }
                    // Removing a running download waits briefly for it to
                    // release its file; keep that off the UI thread.
                    let m = self.m.clone();
                    std::thread::spawn(move || ids.into_iter().for_each(|id| m.remove(id, files)));
                    self.delete = None;
                }
            }
            return;
        }
        if let Some(d) = &mut self.add_dialog {
            match d.show(ctx, &self.m, &self.settings, &self.items, None) {
                DialogResult::Open => {}
                DialogResult::Cancel => self.add_dialog = None,
                DialogResult::Done(req) => {
                    self.clip.note_own_copy(&req.url);
                    let id = self.m.add(req);
                    self.select_only(id);
                    self.add_dialog = None;
                }
            }
            return;
        }
        let pos = (self.capture_total + 1).saturating_sub(self.captures.len()).max(1);
        if let Some(d) = self.captures.front_mut() {
            match d.show(ctx, &self.m, &self.settings, &self.items, Some((pos, self.capture_total))) {
                DialogResult::Open => {}
                DialogResult::Cancel => {
                    self.captures.pop_front();
                }
                DialogResult::Done(req) => {
                    let id = self.m.add(req);
                    self.select_only(id);
                    self.captures.pop_front();
                }
            }
            if self.captures.is_empty() {
                self.capture_total = 0;
            }
            return;
        }
        if let Some(b) = &mut self.batch {
            match b.show(ctx) {
                DialogResult::Open => {}
                DialogResult::Cancel => self.batch = None,
                DialogResult::Done(out) => {
                    let n = out.requests.len();
                    for r in out.requests {
                        self.m.add(r);
                    }
                    self.batch = None;
                    self.toast(ToastKind::Info, "Downloads added", format!("{n} item(s)"), 4);
                }
            }
            return;
        }
        if let Some(o) = &mut self.options {
            let mut host = false;
            let r = o.show(ctx, &mut host);
            if host {
                let tx = self.tx.clone();
                let c = ctx.clone();
                std::thread::spawn(move || {
                    let r = sys::install_browser_host();
                    let _ = tx.send(UiEvent::HostInstalled(r));
                    c.request_repaint();
                });
            }
            match r {
                DialogResult::Open => {}
                DialogResult::Cancel => self.options = None,
                DialogResult::Done(out) => {
                    self.apply_settings(out.settings);
                    self.prefs.theme = out.ui.theme;
                    self.prefs.theme.apply(ctx);
                    self.prefs.minimize_to_tray = out.ui.minimize_to_tray;
                    if out.ui_changed_autostart
                        && let Err(e) = sys::set_autostart(out.ui.autostart)
                    {
                        self.toast(ToastKind::Failed, "Could not change sign-in start", e, 6);
                    }
                    self.options = None;
                }
            }
            return;
        }
        if let Some(u) = &mut self.update {
            let active = self.items.iter().filter(|i| i.status.is_active()).count();
            match u.show(ctx, active) {
                DialogResult::Open => {}
                DialogResult::Cancel => self.update = None,
                DialogResult::Done(UpdateOutcome::Skip(version)) => {
                    let mut s = self.settings.clone();
                    s.skipped_version = Some(version);
                    self.apply_settings(s);
                    self.update = None;
                }
                DialogResult::Done(UpdateOutcome::Restart(exe)) => {
                    let _ = self.restart.set(exe);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    self.update = None;
                }
            }
            return;
        }
        if self.about && AboutDialog::show(ctx) {
            self.about = false;
        }
        if self.shortcuts && crate::dialogs::shortcuts_dialog(ctx) {
            self.shortcuts = false;
        }
    }

    /// Select a download the app just added. Enter does not open it until
    /// the user picks it, so a confirmation cannot turn into opening the file.
    fn select_only(&mut self, id: u64) {
        self.selection.clear();
        self.selection.insert(id);
        self.anchor = Some(id);
        self.focus_item = Some(id);
        self.focus_by_user = false;
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.toasts.retain(|t| now.duration_since(t.created) < t.ttl);
        if self.toasts.is_empty() {
            return;
        }
        let mut close = Vec::new();
        let mut open_path = None;
        let mut reveal_path = None;
        let mut cancel_shutdown = false;
        egui::Area::new(egui::Id::unique("toasts")).anchor(Align2::RIGHT_BOTTOM, vec2(-16.0, -36.0)).order(egui::Order::Foreground).interactable(true).show(ctx, |ui| {
            ui.set_width(340.0);
            for t in self.toasts.iter().rev() {
                let accent = match t.kind {
                    ToastKind::Done { .. } => theme::GOOD,
                    ToastKind::Failed | ToastKind::Shutdown => theme::BAD,
                    ToastKind::Info => theme::ACCENT,
                };
                egui::Frame::popup(ui.style()).corner_radius(10).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                    ui.set_width(316.0);
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(4.0, 36.0), Sense::hover());
                        ui.painter().rect_filled(r, CornerRadius::same(2), accent);
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&t.title).strong());
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui.small_button("×").clicked() {
                                        close.push(t.id);
                                    }
                                });
                            });
                            ui.add(egui::Label::new(&t.body).wrap());
                            match &t.kind {
                                ToastKind::Done { path } => {
                                    ui.horizontal(|ui| {
                                        if ui.button("Open").clicked() {
                                            open_path = Some(path.clone());
                                            close.push(t.id);
                                        }
                                        if ui.button("Show in folder").clicked() {
                                            reveal_path = Some(path.clone());
                                            close.push(t.id);
                                        }
                                    });
                                }
                                ToastKind::Shutdown => {
                                    let left = t.ttl.saturating_sub(now.duration_since(t.created)).as_secs();
                                    if ui.button(format!("Cancel shutdown ({left}s)")).clicked() {
                                        cancel_shutdown = true;
                                        close.push(t.id);
                                    }
                                }
                                _ => {}
                            }
                        });
                    });
                });
                ui.add_space(8.0);
            }
        });
        if let Some(p) = open_path {
            let _ = sys::open_path(&p);
        }
        if let Some(p) = reveal_path {
            let _ = sys::reveal(&p);
        }
        if cancel_shutdown {
            sys::cancel_shutdown();
            self.toast(ToastKind::Info, "Shutdown cancelled", "The computer stays on.".into(), 4);
        }
        self.toasts.retain(|t| !close.contains(&t.id));
        if self.toasts.iter().any(|t| now.duration_since(t.created) < Duration::from_millis(400)) {
            // A new area is measured invisibly on its first frame; make
            // sure the frames that actually show it follow right away.
            ctx.request_repaint_after(Duration::from_millis(30));
        }
        if let Some(next) = self.toasts.iter().map(|t| t.ttl.saturating_sub(now.duration_since(t.created))).min() {
            let tick = if self.toasts.iter().any(|t| matches!(t.kind, ToastKind::Shutdown)) { Duration::from_secs(1) } else { next };
            ctx.request_repaint_after(tick.min(next) + Duration::from_millis(20));
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events(ctx);
        if self.first_frame {
            self.first_frame = false;
            if self.background_start && !self.any_modal() {
                if self.tray.is_some() {
                    self.hidden_to_tray = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                } else {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
            }
        }
        let (minimized, focused) = ctx.input(|i| (i.viewport().minimized.unwrap_or(false), i.viewport().focused.unwrap_or(true)));
        self.window_focused = focused;
        if minimized && self.prefs.minimize_to_tray && self.tray.is_some() && !self.hidden_to_tray {
            self.hidden_to_tray = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        if self.hidden_to_tray {
            // Keep the tray tooltip and notifications flowing while hidden.
            self.refresh(ctx);
            if self.items.iter().any(|i| i.status.is_active()) {
                ctx.request_repaint_after(Duration::from_secs(1));
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.any_modal() {
            ctx.input_mut(|i| drop_held_keys(&mut i.events));
        }
        self.shortcuts(&ctx);
        self.refresh(&ctx);

        egui::Panel::top("menu").frame(egui::Frame::new().fill(theme::toolbar_fill(ui)).inner_margin(egui::Margin::symmetric(8, 3))).show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("toolbar")
            .frame(egui::Frame::new().fill(theme::toolbar_fill(ui)).inner_margin(egui::Margin::symmetric(8, 6)))
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(theme::toolbar_fill(ui)).inner_margin(egui::Margin::symmetric(12, 4)))
            .show(ui, |ui| self.status_bar(ui));
        if self.prefs.show_sidebar {
            egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(190.0)
                .min_size(150.0)
                .max_size(320.0)
                .frame(egui::Frame::new().fill(theme::sidebar_fill(ui)).inner_margin(egui::Margin::symmetric(10, 6)))
                .show(ui, |ui| self.sidebar(ui));
        }
        if self.prefs.show_details && self.focus_item.is_some() {
            egui::Panel::bottom("details")
                .resizable(true)
                .default_size(280.0)
                .min_size(120.0)
                .frame(egui::Frame::new().fill(ui.visuals().panel_fill).inner_margin(egui::Margin::symmetric(14, 6)))
                .show(ui, |ui| self.details(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(ui.visuals().extreme_bg_color).inner_margin(egui::Margin::symmetric(6, 4)))
            .show(ui, |ui| self.table(ui));

        self.dialogs(&ctx);
        self.toasts_ui(&ctx);

        if self.items.iter().any(|i| i.status.is_active()) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, PREFS_KEY, &self.prefs);
    }

    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(60)
    }
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

fn tool_button(ui: &mut egui::Ui, icon: Icon, label: &str, enabled: bool, tip: &str) -> egui::Response {
    let size = vec2(68.0, 56.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if enabled && (resp.hovered() || resp.is_pointer_button_down_on()) {
            let fill = if resp.is_pointer_button_down_on() { ui.visuals().widgets.active.weak_bg_fill } else { ui.visuals().widgets.hovered.weak_bg_fill };
            p.rect_filled(rect, CornerRadius::same(8), fill);
        }
        let icon_rect = egui::Rect::from_center_size(egui::pos2(rect.center().x, rect.top() + 19.0), vec2(26.0, 26.0));
        let fg = ui.visuals().text_color();
        icons::paint_icon(p, icon_rect, icon, fg, enabled);
        let color = if enabled { fg } else { ui.visuals().weak_text_color().gamma_multiply(0.6) };
        p.text(egui::pos2(rect.center().x, rect.bottom() - 10.0), Align2::CENTER_CENTER, label, egui::FontId::proportional(12.0), color);
    }
    resp.on_hover_text(tip)
}

/// Total download rate with a painted arrow (fixed width so the toolbar
/// does not jump around as the number changes).
fn speed_readout(ui: &mut egui::Ui, total: f64) {
    let (rect, resp) = ui.allocate_exact_size(vec2(112.0, 40.0), Sense::hover());
    let active = total >= 1.0;
    let color = if active { if ui.visuals().dark_mode { Color32::from_rgb(110, 170, 255) } else { theme::ACCENT } } else { ui.visuals().weak_text_color() };
    let p = ui.painter();
    let a = egui::Rect::from_center_size(egui::pos2(rect.left() + 9.0, rect.center().y - 6.0), vec2(14.0, 16.0));
    p.rect_filled(egui::Rect::from_min_max(egui::pos2(a.center().x - 2.0, a.top()), egui::pos2(a.center().x + 2.0, a.top() + 9.0)), 0.0, color);
    p.add(egui::Shape::convex_polygon(vec![egui::pos2(a.left(), a.top() + 8.0), egui::pos2(a.right(), a.top() + 8.0), egui::pos2(a.center().x, a.bottom())], color, Stroke::NONE));
    let text = if active { format::speed(total) } else { "Idle".to_string() };
    p.text(egui::pos2(rect.left() + 22.0, rect.center().y - 6.0), Align2::LEFT_CENTER, text, egui::FontId::proportional(17.0), color);
    p.text(egui::pos2(rect.left() + 22.0, rect.center().y + 12.0), Align2::LEFT_CENTER, "total speed", egui::FontId::proportional(11.0), ui.visuals().weak_text_color());
    resp.on_hover_text("Combined speed of all running downloads");
}

fn toolbar_gap(ui: &mut egui::Ui) {
    ui.add_space(6.0);
    let (r, _) = ui.allocate_exact_size(vec2(1.0, 40.0), Sense::hover());
    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color));
    ui.add_space(6.0);
}

fn sidebar_entry(ui: &mut egui::Ui, current: &mut Filter, f: Filter, label: &str, count: usize, cat: Option<Category>) {
    let selected = *current == f;
    let h = 28.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if selected {
            p.rect_filled(rect, CornerRadius::same(6), ui.visuals().selection.bg_fill);
        } else if resp.hovered() {
            p.rect_filled(rect, CornerRadius::same(6), ui.visuals().widgets.hovered.weak_bg_fill);
        }
        let mut x = rect.left() + 8.0;
        let glyph = egui::Rect::from_center_size(egui::pos2(x + 8.0, rect.center().y), vec2(16.0, 16.0));
        match cat {
            Some(c) => icons::paint_category(p, glyph, c),
            None => {
                let color = match f {
                    Filter::Finished => theme::GOOD,
                    Filter::Unfinished => theme::ACCENT,
                    Filter::Queued => theme::WARN,
                    _ => ui.visuals().text_color(),
                };
                if f == Filter::All {
                    paint_logo(p, glyph);
                } else {
                    icons::dot(p, glyph.center(), 5.0, color);
                }
            }
        }
        x += 24.0;
        let text_color = if selected { ui.visuals().strong_text_color() } else { ui.visuals().text_color() };
        p.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, label, egui::FontId::proportional(14.0), text_color);
        if count > 0 {
            p.text(egui::pos2(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, count.to_string(), egui::FontId::proportional(12.0), ui.visuals().weak_text_color());
        }
    }
    if resp.clicked() {
        *current = f;
    }
}

/// The Spate mark (tile + arrow) drawn with the painter.
pub fn paint_logo(p: &egui::Painter, rect: egui::Rect) {
    let w = rect.width();
    p.rect_filled(rect, CornerRadius::same((w * 0.22) as u8), Color32::from_rgb(43, 118, 230));
    let at = |x: f32, y: f32| egui::pos2(rect.left() + x * w, rect.top() + y * w);
    for (y0, y1) in [(0.14, 0.23), (0.27, 0.36), (0.40, 0.50)] {
        p.rect_filled(egui::Rect::from_min_max(at(0.425, y0), at(0.575, y1)), 0.0, Color32::WHITE);
    }
    p.add(egui::Shape::convex_polygon(vec![at(0.25, 0.47), at(0.75, 0.47), at(0.5, 0.73)], Color32::WHITE, Stroke::NONE));
    p.rect_filled(egui::Rect::from_min_max(at(0.22, 0.79), at(0.78, 0.87)), CornerRadius::same((w * 0.04) as u8), Color32::WHITE);
}

fn legend(ui: &mut egui::Ui) {
    let entry = |ui: &mut egui::Ui, color: Color32, text: &str| {
        ui.weak(text);
        let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        ui.painter().rect_filled(r, CornerRadius::same(2), color);
    };
    entry(ui, theme::track_color(ui), "missing");
    entry(ui, theme::ACCENT.gamma_multiply(0.8), "received");
    entry(ui, Color32::from_rgb(92, 166, 255), "receiving now");
}

fn kv(ui: &mut egui::Ui, key: &str, value: impl FnOnce(&mut egui::Ui)) {
    ui.label(RichText::new(key).small().color(ui.visuals().weak_text_color()));
    value(ui);
    ui.end_row();
}

fn context_menu(ui: &mut egui::Ui, it: &ItemView, targets: Vec<u64>, actions: &mut Vec<Action>) {
    let done = it.status == Status::Completed;
    let multi = targets.len() > 1;
    if ui.add_enabled(done && !multi, egui::Button::new("Open")).clicked() {
        actions.push(Action::Open(it.id));
    }
    if cfg!(windows) && ui.add_enabled(done && !multi, egui::Button::new("Open with…")).clicked() {
        actions.push(Action::OpenWith(it.id));
    }
    if ui.add_enabled(!multi, egui::Button::new("Show in folder")).clicked() {
        actions.push(Action::Reveal(it.id));
    }
    if ui.add_enabled(!multi, egui::Button::new("Copy address")).clicked() {
        actions.push(Action::CopyUrl(it.id));
    }
    ui.separator();
    if it.status.is_active() || it.status == Status::Queued {
        if ui.button("Pause").clicked() {
            actions.push(Action::Pause(targets.clone()));
        }
    } else if !done && ui.button("Resume").clicked() {
        actions.push(Action::Resume(targets.clone()));
    }
    if ui.button("Download again").clicked() {
        actions.push(Action::Redownload(targets.clone()));
    }
    if it.queued {
        if ui.button("Take out of queue").clicked() {
            actions.push(Action::Queue(targets.clone(), false));
        }
    } else if !done && ui.button("Move to queue").clicked() {
        actions.push(Action::Queue(targets.clone(), true));
    }
    if !done {
        ui.menu_button("Speed limit", |ui| {
            for kib in [0u64, 128, 256, 512, 1024, 2048, 5120, 10240] {
                if ui.button(format::limit_kib(kib)).clicked() {
                    actions.push(Action::Limit(targets.clone(), (kib > 0).then_some(kib * 1024)));
                }
            }
        });
    }
    ui.separator();
    if ui.button("Delete…").clicked() {
        actions.push(Action::Delete(targets.clone(), false));
    }
    if ui.button("Delete with file…").clicked() {
        actions.push(Action::Delete(targets.clone(), true));
    }
    ui.separator();
    if ui.add_enabled(!multi, egui::Button::new("Details")).clicked() {
        actions.push(Action::Details(it.id));
    }
}

fn empty_state(ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.3).max(20.0));
        let (r, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
        paint_logo(ui.painter(), r);
        ui.add_space(10.0);
        ui.label(RichText::new("No downloads yet").heading().strong());
        ui.weak("Add an address, paste links with Ctrl+V, or let the browser extension hand downloads over.");
        ui.add_space(10.0);
        if ui.add(crate::dialogs::primary_button("Add download")).clicked() {
            actions.push(Action::Details(0));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, repeat: bool, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat, modifiers }
    }

    #[test]
    fn held_enter_opens_nothing() {
        let mut events = vec![key(Key::Enter, true, Modifiers::NONE), key(Key::Enter, true, Modifiers::NONE)];
        assert!(!take_enter(&mut events));
        assert!(events.is_empty(), "repeats are swallowed, not passed on");

        let mut events = vec![key(Key::A, false, Modifiers::NONE), key(Key::Enter, false, Modifiers::NONE)];
        assert!(take_enter(&mut events));
        assert_eq!(events.len(), 1);

        let mut events = vec![key(Key::Enter, false, Modifiers::COMMAND)];
        assert!(!take_enter(&mut events));
        assert_eq!(events.len(), 1, "Ctrl+Enter is left alone");
    }

    #[test]
    fn held_keys_cannot_click_dialog_buttons() {
        let mut events = vec![
            key(Key::Enter, true, Modifiers::NONE),
            key(Key::Space, true, Modifiers::NONE),
            key(Key::Enter, false, Modifiers::NONE),
            key(Key::A, true, Modifiers::NONE),
            egui::Event::Text(" ".into()),
        ];
        drop_held_keys(&mut events);
        assert_eq!(
            events,
            vec![key(Key::Enter, false, Modifiers::NONE), key(Key::A, true, Modifiers::NONE), egui::Event::Text(" ".into())]
        );
    }

    #[test]
    fn enter_opens_only_what_the_user_picked() {
        let selection: BTreeSet<u64> = [7].into();
        assert_eq!(enter_target(Some(7), &selection, true), Some(7));
        // Just added from a dialog: selected by the app, not opened by Enter.
        assert_eq!(enter_target(Some(7), &selection, false), None);
        assert_eq!(enter_target(Some(8), &selection, true), None);
        assert_eq!(enter_target(None, &selection, true), None);
    }
}
