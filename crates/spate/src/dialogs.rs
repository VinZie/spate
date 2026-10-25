//! Modal dialogs: add URL / captured download, batch add, delete
//! confirmation, options, update and about.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Id, Layout, RichText, vec2};
use spate_core::http::NetConfig;
use spate_core::ipc::AddItem;
use spate_core::manager::is_credential_header;
use spate_core::naming;
use spate_core::settings::Schedule;
use spate_core::{AddRequest, Category, DownloadKind, ItemView, Manager, ProbeInfo, Settings, StartMode};

use crate::theme::{self, ThemeChoice};
use crate::update::{self, Check, Progress, Release};
use crate::{format, icons, sys, urls};

/// Frame for every modal dialog.
fn dialog_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::popup(&ctx.global_style()).inner_margin(egui::Margin::same(18)).corner_radius(10)
}

fn title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).heading().strong());
    ui.add_space(4.0);
}

pub fn primary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(Color32::WHITE).strong()).fill(theme::ACCENT).min_size(vec2(96.0, 30.0))
}

pub fn secondary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(text).min_size(vec2(88.0, 30.0))
}

fn folder_row(ui: &mut egui::Ui, dir: &mut String, edited: &mut bool) {
    ui.horizontal(|ui| {
        let w = ui.available_width() - 92.0;
        if ui.add(egui::TextEdit::singleline(dir).desired_width(w)).changed() {
            *edited = true;
        }
        if ui.button("Browse…").clicked() {
            let mut dlg = rfd::FileDialog::new().set_title("Choose a folder");
            if !dir.trim().is_empty() && std::path::Path::new(dir.trim()).is_dir() {
                dlg = dlg.set_directory(dir.trim());
            }
            if let Some(p) = dlg.pick_folder() {
                *dir = p.display().to_string();
                *edited = true;
            }
        }
    });
}

fn kind_from_item(item: &AddItem) -> Option<DownloadKind> {
    match item.kind.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("hls") => Some(DownloadKind::Hls),
        Some("http") => Some(DownloadKind::Http),
        _ => item.mime.as_deref().filter(|m| is_hls_mime(m)).map(|_| DownloadKind::Hls),
    }
}

fn is_hls_mime(mime: &str) -> bool {
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    matches!(mime.as_str(), "application/vnd.apple.mpegurl" | "application/x-mpegurl" | "audio/mpegurl" | "audio/x-mpegurl")
}

/// The name a browser/CLI item is saved under, as the dialogs show it.
fn item_name(item: &AddItem) -> String {
    match item.file_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => naming::sanitize(name),
        None => name_from_url(&item.url),
    }
}

fn name_from_url(url: &str) -> String {
    naming::sanitize(&urls::file_name_from_url(url.trim()))
}

/// Request for a browser/CLI item that is added without asking.
pub fn request_from_item(item: &AddItem, start: StartMode) -> AddRequest {
    AddRequest {
        url: item.url.trim().to_string(),
        headers: item.request_headers(),
        file_name: item.file_name.clone().filter(|n| !n.trim().is_empty()),
        dir: item.dir.clone(),
        start,
        kind: kind_from_item(item),
        ..Default::default()
    }
}

/// Whether Enter was pressed this frame. Key repeat does not count: holding
/// Enter down must not confirm a dialog that appears meanwhile.
pub fn enter_pressed(events: &[egui::Event]) -> bool {
    events.iter().any(|e| matches!(e, egui::Event::Key { key: egui::Key::Enter, pressed: true, repeat: false, .. }))
}

const WAIT_HINT: &str = "Available in a moment";

/// Keeps a dialog that opened by itself (a download from the browser, a
/// link from the clipboard watcher) from being confirmed by a click or an
/// Enter meant for something else: its confirm actions only work once it
/// has been showing in the focused window for [`ConfirmGate::DELAY`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ConfirmGate {
    /// Since when the dialog has been showing with the window focused.
    since: Option<Instant>,
}

impl ConfirmGate {
    pub const DELAY: Duration = Duration::from_millis(1000);

    /// Call on every frame the dialog is shown. Returns whether it may be
    /// confirmed now; losing the focus starts the wait over.
    pub fn update(&mut self, now: Instant, focused: bool) -> bool {
        if !focused {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        now.duration_since(since) >= Self::DELAY
    }

    /// Time left until [`ConfirmGate::update`] allows confirming.
    pub fn remaining(&self, now: Instant) -> Duration {
        self.since.map_or(Self::DELAY, |s| Self::DELAY.saturating_sub(now.duration_since(s)))
    }
}

/// `gate` updated for this frame (no gate: always open), asking for a
/// repaint for when it opens.
fn gate_open(gate: &mut Option<ConfirmGate>, ctx: &egui::Context) -> bool {
    let Some(gate) = gate else { return true };
    let now = Instant::now();
    let open = gate.update(now, ctx.input(|i| i.focused));
    if !open {
        ctx.request_repaint_after(gate.remaining(now) + Duration::from_millis(20));
    }
    open
}

/// `name` shortened in the middle to fit `width`, so that the extension
/// stays visible however long the name is.
pub fn fit_name(ui: &egui::Ui, name: &str, font: &egui::FontId, width: f32) -> String {
    let fits = |s: &str| ui.painter().layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x <= width;
    if fits(name) {
        return name.to_string();
    }
    let (mut lo, mut hi) = (0, name.chars().count());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits(&format::middle_ellipsis(name, mid)) { lo = mid } else { hi = mid - 1 }
    }
    format::middle_ellipsis(name, lo)
}

/// A file name in bold, shortened in the middle when long (full name on
/// hover).
fn name_label(ui: &mut egui::Ui, name: &str) -> egui::Response {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let shown = fit_name(ui, name, &font, ui.available_width());
    let r = ui.add(egui::Label::new(RichText::new(shown.as_str()).strong()).wrap_mode(egui::TextWrapMode::Extend));
    if shown != name { r.on_hover_text(name) } else { r }
}

// ---------------------------------------------------------------------------
// Add URL / captured download
// ---------------------------------------------------------------------------

/// Where the address in an [`AddDialog`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Typed by the user; the dialog opens empty.
    Typed,
    /// Taken from the clipboard because the user asked to add a download.
    Clipboard,
    /// Noticed on the clipboard by the watcher; the dialog opened by itself.
    ClipboardWatch,
    /// Handed over by the browser extension, the CLI or the command line.
    Capture,
}

impl Source {
    /// Whether the dialog opened without the user asking for it.
    fn unprompted(self) -> bool {
        matches!(self, Source::ClipboardWatch | Source::Capture)
    }
}

/// Whether a dialog checks its address with the server before the user has
/// confirmed. An address the user did not type (the clipboard can be written
/// by any web page) is only checked when asked to. A captured download is
/// only checked if it came without credentials: the check never sends them
/// (see [`check_headers`]), so it would describe a login page rather than
/// the file. Editing the address always checks it.
fn checks_on_its_own(source: Source, headers: &[(String, String)]) -> bool {
    match source {
        Source::Typed => true,
        Source::Clipboard | Source::ClipboardWatch => false,
        Source::Capture => !headers.iter().any(|(name, _)| is_credential_header(name)),
    }
}

/// Headers for checking an address before the download is confirmed:
/// never cookies or other credentials.
fn check_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers.iter().filter(|(name, _)| !is_credential_header(name)).cloned().collect()
}

enum Probe {
    Idle,
    Pending(tokio::sync::oneshot::Receiver<spate_core::Result<ProbeInfo>>),
    Done(ProbeInfo),
    Failed(String),
}

pub struct AddDialog {
    id: Id,
    source: Source,
    url: String,
    headers: Vec<(String, String)>,
    file_name: String,
    name_edited: bool,
    dir: String,
    dir_edited: bool,
    connections: usize,
    limit_kib: u64,
    sha256: String,
    browser_size: Option<u64>,
    browser_mime: Option<String>,
    kind: Option<DownloadKind>,
    probe: Probe,
    url_changed: Option<Instant>,
    focus_url: bool,
    gate: Option<ConfirmGate>,
    error: Option<String>,
}

pub enum DialogResult<T> {
    Open,
    Cancel,
    Done(T),
}

impl AddDialog {
    /// A dialog for `url` (empty for [`Source::Typed`]).
    pub fn new(url: String, source: Source, settings: &Settings) -> Self {
        Self::unchecked(url, source, settings).check_if_allowed()
    }

    fn unchecked(url: String, source: Source, settings: &Settings) -> Self {
        let name = if url.is_empty() { String::new() } else { name_from_url(&url) };
        let dir = settings.dir_for(Category::for_file_name(&name)).display().to_string();
        let has_url = urls::is_http_url(&url);
        Self {
            id: Id::unique(("add_dialog", Instant::now())),
            source,
            url,
            headers: Vec::new(),
            file_name: name,
            name_edited: false,
            dir,
            dir_edited: false,
            connections: settings.connections,
            limit_kib: 0,
            sha256: String::new(),
            browser_size: None,
            browser_mime: None,
            kind: None,
            probe: Probe::Idle,
            url_changed: None,
            focus_url: !has_url,
            gate: source.unprompted().then(ConfirmGate::default),
            error: None,
        }
    }

    pub fn from_capture(item: &AddItem, settings: &Settings) -> Self {
        let mut d = Self::unchecked(item.url.trim().to_string(), Source::Capture, settings);
        d.headers = item.request_headers();
        if let Some(name) = item.file_name.as_deref().filter(|n| !n.trim().is_empty()) {
            d.file_name = naming::sanitize(name.trim());
            d.name_edited = true;
            d.dir = settings.dir_for(Category::for_file_name(&d.file_name)).display().to_string();
        }
        if let Some(dir) = &item.dir {
            d.dir = dir.display().to_string();
            d.dir_edited = true;
        }
        d.browser_size = item.size;
        d.browser_mime = item.mime.clone().filter(|m| !m.trim().is_empty());
        d.kind = kind_from_item(item);
        d.focus_url = false;
        d.check_if_allowed()
    }

    /// Check a prefilled address right away if that is allowed.
    fn check_if_allowed(mut self) -> Self {
        if urls::is_http_url(&self.url) && checks_on_its_own(self.source, &self.headers) {
            self.check_now();
        }
        self
    }

    fn check_now(&mut self) {
        self.url_changed = Some(Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now));
    }

    fn poll_probe(&mut self, manager: &Manager, settings: &Settings, ctx: &egui::Context) {
        let url = self.url.trim().to_string();
        if let Some(t) = self.url_changed {
            if t.elapsed() >= Duration::from_millis(450) && urls::is_http_url(&url) {
                self.url_changed = None;
                self.probe = Probe::Pending(manager.probe(url, check_headers(&self.headers)));
            } else if t.elapsed() < Duration::from_millis(450) {
                ctx.request_repaint_after(Duration::from_millis(120));
            }
        }
        if let Probe::Pending(rx) = &mut self.probe {
            match rx.try_recv() {
                Ok(Ok(info)) => {
                    if !self.name_edited && !info.file_name.is_empty() {
                        self.file_name = info.file_name.clone();
                    }
                    if self.kind.is_none() {
                        self.kind = Some(info.kind);
                    }
                    if !self.dir_edited {
                        self.dir = settings.dir_for(Category::for_file_name(&self.file_name)).display().to_string();
                    }
                    self.probe = Probe::Done(info);
                }
                Ok(Err(e)) => self.probe = Probe::Failed(e.to_string()),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => self.probe = Probe::Failed("The check was interrupted.".into()),
            }
        }
    }

    fn request(&self, start: StartMode) -> AddRequest {
        let probe_named = matches!(self.probe, Probe::Done(_));
        let capture = self.source == Source::Capture;
        AddRequest {
            url: self.url.trim().to_string(),
            headers: self.headers.clone(),
            // Always the name shown, even if it is the one in the address:
            // otherwise the server's suggestion would replace what the user
            // confirmed (`invoice.pdf` saved as `invoice.exe`).
            file_name: Some(self.file_name.trim().to_string()).filter(|n| !n.is_empty()),
            dir: (self.dir_edited || probe_named || capture).then(|| PathBuf::from(self.dir.trim())).filter(|d| !d.as_os_str().is_empty()),
            start,
            connections: Some(self.connections),
            speed_limit: (self.limit_kib > 0).then_some(self.limit_kib * 1024),
            sha256: Some(self.sha256.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()),
            kind: self.kind,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if !urls::is_http_url(self.url.trim()) {
            return Err("Enter an http:// or https:// address.".into());
        }
        if self.file_name.trim().is_empty() {
            return Err("Enter a file name.".into());
        }
        if self.dir.trim().is_empty() {
            return Err("Choose a folder to save into.".into());
        }
        let sha = self.sha256.trim();
        if !sha.is_empty() && (sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit())) {
            return Err("A SHA-256 checksum has 64 hexadecimal digits.".into());
        }
        Ok(())
    }

    pub fn show(&mut self, ctx: &egui::Context, manager: &Manager, settings: &Settings, items: &[ItemView], position: Option<(usize, usize)>) -> DialogResult<AddRequest> {
        self.poll_probe(manager, settings, ctx);
        let armed = gate_open(&mut self.gate, ctx);
        let mut result = DialogResult::Open;
        let modal = egui::Modal::new(self.id).frame(dialog_frame(ctx)).show(ctx, |ui| {
            ui.set_width(560.0);
            let heading = if self.source == Source::Capture { "Confirm download" } else { "Add download" };
            ui.horizontal(|ui| {
                title(ui, heading);
                if let Some((i, n)) = position.filter(|(_, n)| *n > 1) {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.weak(format!("{i} of {n}")));
                }
            });

            egui::Grid::new(self.id.with("grid")).num_columns(2).spacing(vec2(12.0, 10.0)).show(ui, |ui| {
                ui.label("Address");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.url)
                        .hint_text("https://example.com/file.zip")
                        .desired_width(f32::INFINITY),
                );
                if self.focus_url {
                    r.request_focus();
                    self.focus_url = false;
                }
                if r.changed() {
                    self.url_changed = Some(Instant::now());
                    if !self.name_edited {
                        self.file_name = name_from_url(&self.url);
                        if !self.dir_edited {
                            self.dir = settings.dir_for(Category::for_file_name(&self.file_name)).display().to_string();
                        }
                    }
                    self.probe = Probe::Idle;
                }
                ui.end_row();

                ui.label("File name");
                if ui.add(egui::TextEdit::singleline(&mut self.file_name).desired_width(f32::INFINITY)).changed() {
                    self.name_edited = true;
                    if !self.dir_edited {
                        self.dir = settings.dir_for(Category::for_file_name(&self.file_name)).display().to_string();
                    }
                }
                ui.end_row();

                ui.label("Save to");
                folder_row(ui, &mut self.dir, &mut self.dir_edited);
                ui.end_row();

                ui.label("Connections");
                ui.add(egui::Slider::new(&mut self.connections, 1..=32).clamping(egui::SliderClamping::Always));
                ui.end_row();
            });

            ui.add_space(8.0);
            self.info_card(ui, items);

            egui::CollapsingHeader::new("More options").id_salt(self.id.with("more")).show(ui, |ui| {
                egui::Grid::new(self.id.with("more_grid")).num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
                    ui.label("Speed limit");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.limit_kib).range(0..=10_000_000).suffix(" KB/s"));
                        ui.weak(if self.limit_kib == 0 { "no limit for this file" } else { "" });
                    });
                    ui.end_row();
                    ui.label("SHA-256");
                    ui.add(egui::TextEdit::singleline(&mut self.sha256).hint_text("optional, verified when finished").desired_width(f32::INFINITY));
                    ui.end_row();
                    if !self.headers.is_empty() {
                        ui.label("Request");
                        ui.weak(format!("{} header(s) from the browser (cookies, referrer…)", self.headers.len()));
                        ui.end_row();
                    }
                });
            });

            if let Some(e) = &self.error {
                ui.colored_label(theme::BAD, e);
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let (now_label, later_label) =
                    if self.source == Source::Capture { ("Start", "Later") } else { ("Start now", "Add to queue") };
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(secondary_button("Cancel")).clicked() {
                        result = DialogResult::Cancel;
                    }
                    let later = ui
                        .add_enabled(armed, secondary_button(later_label))
                        .on_hover_text("Add to the queue; it starts when the queue runs")
                        .on_disabled_hover_text(WAIT_HINT);
                    if later.clicked() {
                        match self.validate() {
                            Ok(()) => result = DialogResult::Done(self.request(StartMode::Queue)),
                            Err(e) => self.error = Some(e),
                        }
                    }
                    let enter = armed && ui.input(|i| enter_pressed(&i.events));
                    if ui.add_enabled(armed, primary_button(now_label)).on_disabled_hover_text(WAIT_HINT).clicked() || enter {
                        match self.validate() {
                            Ok(()) => result = DialogResult::Done(self.request(StartMode::Now)),
                            Err(e) => self.error = Some(e),
                        }
                    }
                });
            });
        });
        if modal.should_close() && matches!(result, DialogResult::Open) && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = DialogResult::Cancel;
        }
        result
    }

    fn info_card(&mut self, ui: &mut egui::Ui, items: &[ItemView]) {
        egui::Frame::group(ui.style()).fill(ui.visuals().faint_bg_color).corner_radius(8).inner_margin(10).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                // The name exactly as it will be saved, extension in view.
                let name = if self.file_name.trim().is_empty() { String::new() } else { naming::sanitize(self.file_name.trim()) };
                let cat = Category::for_file_name(&name);
                icons::category_glyph(ui, cat, 30.0);
                ui.vertical(|ui| {
                    if name.is_empty() {
                        ui.label(RichText::new("–").strong());
                    } else {
                        name_label(ui, &name);
                    }
                    let unchecked = matches!(self.probe, Probe::Idle) && self.url_changed.is_none();
                    let line = match &self.probe {
                        Probe::Idle if !urls::is_http_url(self.url.trim()) => RichText::new("Paste or type a download address.").weak(),
                        Probe::Idle if unchecked => {
                            // Not checked with the server: what is known so far.
                            let mut parts: Vec<String> = self.browser_size.map(format::size).into_iter().collect();
                            parts.push(cat.label().to_string());
                            parts.extend(self.browser_mime.clone());
                            parts.push(if self.source == Source::Capture { "not checked" } else { "not checked yet" }.into());
                            RichText::new(parts.join(" · ")).weak()
                        }
                        Probe::Idle | Probe::Pending(_) => {
                            let mut t = "Checking the address…".to_string();
                            if let Some(s) = self.browser_size {
                                t = format!("{} · {t}", format::size(s));
                            }
                            RichText::new(t).weak()
                        }
                        Probe::Done(info) => {
                            let size = info.total.or(self.browser_size).map(format::size).unwrap_or_else(|| "Unknown size".into());
                            let resume = if info.resumable { "can resume, splits into parts" } else { "no resume support, single connection" };
                            let mut t = format!("{size} · {} · {resume}", cat.label());
                            if info.kind == DownloadKind::Hls {
                                t.push_str(" · HLS stream");
                            }
                            if let Some(ct) = &info.content_type {
                                t.push_str(&format!(" · {ct}"));
                            }
                            RichText::new(t)
                        }
                        Probe::Failed(e) => RichText::new(format!("Could not check the address: {e}")).color(theme::WARN),
                    };
                    if unchecked && self.source != Source::Capture && urls::is_http_url(self.url.trim()) {
                        ui.horizontal(|ui| {
                            ui.label(line);
                            if ui.small_button("Check").on_hover_text("Ask the server for the name and size").clicked() {
                                self.check_now();
                            }
                        });
                    } else {
                        ui.label(line);
                    }
                    if let Probe::Done(info) = &self.probe
                        && info.final_url != self.url.trim()
                        && !info.final_url.is_empty()
                    {
                        let host = info.final_url.split_once("://").map(|(_, r)| r).unwrap_or(&info.final_url);
                        let host = host.split(['/', '?', '#']).next().unwrap_or(host);
                        ui.add(egui::Label::new(RichText::new(format!("Served from {host}")).weak()).truncate()).on_hover_text(&info.final_url);
                    }
                    let url = self.url.trim();
                    if let Some(existing) = items.iter().find(|i| !url.is_empty() && i.url == url) {
                        ui.colored_label(theme::WARN, format!("Already in the list as “{}” ({}).", existing.file_name, existing.status.label()));
                    }
                });
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Batch add
// ---------------------------------------------------------------------------

pub struct BatchEntry {
    pub checked: bool,
    pub item: AddItem,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum BatchSource {
    List,
    Pattern,
}

pub struct BatchDialog {
    id: Id,
    entries: Vec<BatchEntry>,
    /// Lets the user type/paste more addresses (not for browser captures).
    editable: bool,
    source: BatchSource,
    paste: String,
    pattern: String,
    from: String,
    to: String,
    width: usize,
    note: Option<String>,
    one_folder: bool,
    dir: String,
    dir_edited: bool,
    connections: usize,
    gate: Option<ConfirmGate>,
}

pub struct BatchOutcome {
    pub requests: Vec<AddRequest>,
}

impl BatchDialog {
    /// `editable` lists are the user's own; the others came from the browser
    /// and so opened by themselves.
    pub fn new(items: Vec<AddItem>, editable: bool, settings: &Settings) -> Self {
        Self {
            id: Id::unique(("batch_dialog", Instant::now())),
            entries: items.into_iter().map(|item| BatchEntry { checked: true, item }).collect(),
            gate: (!editable).then(ConfirmGate::default),
            editable,
            source: BatchSource::List,
            paste: String::new(),
            pattern: String::new(),
            from: "1".into(),
            to: "10".into(),
            width: 0,
            note: None,
            one_folder: false,
            dir: settings.download_dir.display().to_string(),
            dir_edited: false,
            connections: settings.connections,
        }
    }

    pub fn from_urls(list: Vec<String>, settings: &Settings) -> Self {
        let items = list.into_iter().map(|url| AddItem { url, ..Default::default() }).collect();
        Self::new(items, true, settings)
    }

    pub fn extend(&mut self, items: Vec<AddItem>) {
        for item in items {
            if !self.entries.iter().any(|e| e.item.url == item.url) {
                self.entries.push(BatchEntry { checked: true, item });
            }
        }
    }

    fn add_urls(&mut self, list: Vec<String>) -> usize {
        let before = self.entries.len();
        self.extend(list.into_iter().map(|url| AddItem { url, ..Default::default() }).collect());
        self.entries.len() - before
    }

    pub fn show(&mut self, ctx: &egui::Context) -> DialogResult<BatchOutcome> {
        let armed = gate_open(&mut self.gate, ctx);
        let mut result = DialogResult::Open;
        let modal = egui::Modal::new(self.id).frame(dialog_frame(ctx)).show(ctx, |ui| {
            ui.set_width(640.0);
            title(ui, if self.editable { "Add several downloads" } else { "Downloads from browser" });

            if self.editable {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.source, BatchSource::List, "Paste addresses");
                    ui.selectable_value(&mut self.source, BatchSource::Pattern, "Numbered pattern");
                });
                ui.add_space(4.0);
                match self.source {
                    BatchSource::List => {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.paste)
                                .hint_text("One address per line (any text containing links works too)")
                                .desired_rows(3)
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Add to list").clicked() {
                                let found = urls::extract_urls(&self.paste);
                                let n = self.add_urls(found);
                                self.note = Some(format!("{n} new address(es) added."));
                                self.paste.clear();
                            }
                            if ui.button("From clipboard").clicked() {
                                let text = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
                                let n = self.add_urls(urls::extract_urls(&text));
                                self.note = Some(if n == 0 { "No new addresses in the clipboard.".into() } else { format!("{n} address(es) added from the clipboard.") });
                            }
                        });
                    }
                    BatchSource::Pattern => {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.pattern)
                                .hint_text("https://example.com/photos/img*.jpg")
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            ui.label("From");
                            ui.add(egui::TextEdit::singleline(&mut self.from).desired_width(60.0));
                            ui.label("to");
                            ui.add(egui::TextEdit::singleline(&mut self.to).desired_width(60.0));
                            ui.label("digits");
                            ui.add(egui::DragValue::new(&mut self.width).range(0..=8));
                            if ui.button("Generate").clicked() {
                                match urls::expand_pattern(self.pattern.trim(), &self.from, &self.to, self.width) {
                                    Ok(list) => {
                                        let n = self.add_urls(list);
                                        self.note = Some(format!("{n} address(es) generated."));
                                    }
                                    Err(e) => self.note = Some(e),
                                }
                            }
                        });
                        ui.weak("The * is replaced by each number (zero-padded to the digit count) or letter in the range.");
                    }
                }
                if let Some(n) = &self.note {
                    ui.weak(n);
                }
                ui.separator();
            }

            let checked = self.entries.iter().filter(|e| e.checked).count();
            ui.horizontal(|ui| {
                ui.strong(format!("{checked} of {} selected", self.entries.len()));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Select none").clicked() {
                        self.entries.iter_mut().for_each(|e| e.checked = false);
                    }
                    if ui.small_button("Select all").clicked() {
                        self.entries.iter_mut().for_each(|e| e.checked = true);
                    }
                });
            });
            egui::Frame::group(ui.style()).inner_margin(6).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(240.0).min_scrolled_height(120.0).auto_shrink([false, false]).show(ui, |ui| {
                    if self.entries.is_empty() {
                        ui.weak("No addresses yet.");
                    }
                    let mut remove = None;
                    for (i, e) in self.entries.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut e.checked, "");
                            let name = item_name(&e.item);
                            icons::category_glyph(ui, Category::for_file_name(&name), 16.0);
                            let font = egui::TextStyle::Body.resolve(ui.style());
                            let shown = fit_name(ui, &name, &font, ui.available_width() * 0.55);
                            ui.add(egui::Label::new(RichText::new(shown).strong()).wrap_mode(egui::TextWrapMode::Extend))
                                .on_hover_text(format!("{name}\n{}", e.item.url));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if self.editable && ui.small_button("×").on_hover_text("Remove from this list").clicked() {
                                    remove = Some(i);
                                }
                                if let Some(s) = e.item.size {
                                    ui.weak(format::size(s));
                                }
                                ui.add(egui::Label::new(RichText::new(&e.item.url).weak().small()).truncate());
                            });
                        });
                    }
                    if let Some(i) = remove {
                        self.entries.remove(i);
                    }
                });
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.one_folder, false, "Sort into category folders");
                ui.radio_value(&mut self.one_folder, true, "Save all into one folder");
            });
            if self.one_folder {
                folder_row(ui, &mut self.dir, &mut self.dir_edited);
            }
            ui.horizontal(|ui| {
                ui.label("Connections per file");
                ui.add(egui::Slider::new(&mut self.connections, 1..=32));
            });

            ui.add_space(10.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(secondary_button("Cancel")).clicked() {
                    result = DialogResult::Cancel;
                }
                let any = checked > 0;
                let hint = |r: egui::Response| if armed { r } else { r.on_disabled_hover_text(WAIT_HINT) };
                if hint(ui.add_enabled(any && armed, secondary_button("Add to queue"))).clicked() {
                    result = DialogResult::Done(self.outcome(StartMode::Queue));
                }
                if hint(ui.add_enabled(any && armed, primary_button("Start all"))).clicked() {
                    result = DialogResult::Done(self.outcome(StartMode::Now));
                }
            });
        });
        if modal.should_close() && matches!(result, DialogResult::Open) && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = DialogResult::Cancel;
        }
        result
    }

    fn outcome(&self, start: StartMode) -> BatchOutcome {
        let dir = self.one_folder.then(|| PathBuf::from(self.dir.trim())).filter(|d| !d.as_os_str().is_empty());
        let requests = self
            .entries
            .iter()
            .filter(|e| e.checked)
            .map(|e| {
                let mut r = request_from_item(&e.item, start);
                // The name the list showed is the name saved.
                r.file_name = Some(item_name(&e.item));
                if dir.is_some() {
                    r.dir = dir.clone();
                }
                r.connections = Some(self.connections);
                r
            })
            .collect();
        BatchOutcome { requests }
    }
}

// ---------------------------------------------------------------------------
// Delete confirmation
// ---------------------------------------------------------------------------

pub struct DeleteDialog {
    pub ids: Vec<u64>,
    names: Vec<String>,
    unfinished: usize,
    delete_files: bool,
    completed_only: bool,
}

impl DeleteDialog {
    pub fn new(ids: Vec<u64>, items: &[ItemView], completed_only: bool) -> Self {
        let chosen: Vec<&ItemView> = items.iter().filter(|i| ids.contains(&i.id)).collect();
        Self {
            names: chosen.iter().map(|i| i.file_name.clone()).collect(),
            unfinished: chosen.iter().filter(|i| i.status != spate_core::Status::Completed).count(),
            ids,
            delete_files: false,
            completed_only,
        }
    }

    /// Pre-tick "also delete the files" (context menu "Delete with file").
    pub fn preset_delete_files(&mut self) {
        self.delete_files = true;
    }

    /// `Done(delete_files)` when confirmed.
    pub fn show(&mut self, ctx: &egui::Context) -> DialogResult<bool> {
        let mut result = DialogResult::Open;
        let modal = egui::Modal::new(Id::unique("delete_dialog")).frame(dialog_frame(ctx)).show(ctx, |ui| {
            ui.set_width(420.0);
            let n = self.ids.len();
            title(ui, if self.completed_only { "Remove completed downloads" } else { "Delete downloads" });
            if n == 1 {
                let name = self.names.first().map(String::as_str).unwrap_or("");
                let font = egui::TextStyle::Body.resolve(ui.style());
                let shown = fit_name(ui, name, &font, ui.available_width() - 160.0);
                ui.label(format!("Remove “{shown}” from the list?")).on_hover_text(name);
            } else {
                ui.label(format!("Remove {n} downloads from the list?"));
            }
            if self.unfinished > 0 {
                ui.colored_label(theme::WARN, if self.unfinished == 1 && n == 1 { "This download is not finished.".to_string() } else { format!("{} of them are not finished.", self.unfinished) });
            }
            ui.add_space(6.0);
            ui.checkbox(&mut self.delete_files, "Also delete the files from disk");
            if self.delete_files {
                ui.colored_label(theme::BAD, "Files are deleted permanently, not moved to the Recycle Bin.");
            }
            ui.add_space(10.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(secondary_button("Cancel")).clicked() {
                    result = DialogResult::Cancel;
                }
                let label = if self.delete_files { "Delete files" } else { "Remove" };
                let btn = egui::Button::new(RichText::new(label).color(Color32::WHITE).strong()).fill(theme::BAD).min_size(vec2(96.0, 30.0));
                if ui.add(btn).clicked() || ui.input(|i| enter_pressed(&i.events)) {
                    result = DialogResult::Done(self.delete_files);
                }
            });
        });
        if modal.should_close() && matches!(result, DialogResult::Open) {
            result = DialogResult::Cancel;
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OptionsTab {
    General,
    Connection,
    Scheduler,
    Browser,
    About,
}

/// UI-only preferences the options window edits.
#[derive(Clone, Debug, PartialEq)]
pub struct UiOptions {
    pub theme: ThemeChoice,
    pub minimize_to_tray: bool,
    pub autostart: bool,
}

pub struct OptionsDialog {
    pub tab: OptionsTab,
    draft: Settings,
    proxy: String,
    ui_draft: UiOptions,
    ui_initial: UiOptions,
    pub host_running: bool,
    pub host_output: Option<Result<String, String>>,
    tray_available: bool,
    error: Option<String>,
}

pub struct OptionsOutcome {
    pub settings: Settings,
    pub ui: UiOptions,
    pub ui_changed_autostart: bool,
}

impl OptionsDialog {
    pub fn new(tab: OptionsTab, settings: &Settings, ui: UiOptions, tray_available: bool) -> Self {
        Self {
            tab,
            proxy: settings.net.proxy.clone().unwrap_or_default(),
            draft: settings.clone(),
            ui_initial: ui.clone(),
            ui_draft: ui,
            host_running: false,
            host_output: None,
            tray_available,
            error: None,
        }
    }

    /// Returns `Done` on OK; `host_requested` is set when the user asked to
    /// register the browser helper.
    pub fn show(&mut self, ctx: &egui::Context, host_requested: &mut bool) -> DialogResult<OptionsOutcome> {
        let mut result = DialogResult::Open;
        let modal = egui::Modal::new(Id::unique("options_dialog")).frame(dialog_frame(ctx)).show(ctx, |ui| {
            ui.set_width(660.0);
            title(ui, "Options");
            ui.horizontal(|ui| {
                for (tab, label) in [
                    (OptionsTab::General, "General"),
                    (OptionsTab::Connection, "Connection"),
                    (OptionsTab::Scheduler, "Scheduler"),
                    (OptionsTab::Browser, "Browser integration"),
                    (OptionsTab::About, "About"),
                ] {
                    ui.selectable_value(&mut self.tab, tab, label);
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().max_height(400.0).min_scrolled_height(400.0).auto_shrink([false, false]).show(ui, |ui| {
                ui.set_min_height(380.0);
                match self.tab {
                    OptionsTab::General => self.general(ui),
                    OptionsTab::Connection => self.connection(ui),
                    OptionsTab::Scheduler => self.scheduler(ui),
                    OptionsTab::Browser => self.browser(ui, host_requested),
                    OptionsTab::About => about_contents(ui),
                }
            });
            if let Some(e) = &self.error {
                ui.colored_label(theme::BAD, e);
            }
            ui.separator();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(secondary_button("Cancel")).clicked() {
                    result = DialogResult::Cancel;
                }
                if ui.add(primary_button("OK")).clicked() {
                    let mut s = self.draft.clone();
                    s.net.proxy = Some(self.proxy.trim().to_string()).filter(|p| !p.is_empty());
                    if s.net.user_agent.trim().is_empty() {
                        s.net.user_agent = spate_core::http::DEFAULT_USER_AGENT.to_string();
                    }
                    if s.download_dir.as_os_str().is_empty() {
                        self.error = Some("Choose a download folder.".into());
                    } else {
                        result = DialogResult::Done(OptionsOutcome {
                            settings: s.normalized(),
                            ui_changed_autostart: self.ui_draft.autostart != self.ui_initial.autostart,
                            ui: self.ui_draft.clone(),
                        });
                    }
                }
            });
        });
        if modal.should_close() && matches!(result, DialogResult::Open) && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = DialogResult::Cancel;
        }
        result
    }

    fn general(&mut self, ui: &mut egui::Ui) {
        section(ui, "Saving files");
        let mut dir = self.draft.download_dir.display().to_string();
        let mut edited = false;
        ui.label("Download folder");
        folder_row(ui, &mut dir, &mut edited);
        if edited {
            self.draft.download_dir = PathBuf::from(dir.trim());
        }
        ui.checkbox(&mut self.draft.category_folders, "Sort new files into a sub-folder per category");
        if self.draft.category_folders {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for c in Category::ALL {
                    icons::category_glyph(ui, c, 14.0);
                    ui.weak(format!("{}{}   ", std::path::MAIN_SEPARATOR, c.folder()));
                }
            });
        }

        section(ui, "On start");
        ui.checkbox(&mut self.draft.start_queue_on_launch, "Start processing the queue when Spate starts");
        ui.checkbox(&mut self.draft.resume_on_launch, "Resume downloads that were running when Spate closed");
        if cfg!(windows) {
            ui.checkbox(&mut self.ui_draft.autostart, "Start Spate in the background when I sign in");
        }

        section(ui, "Behaviour");
        ui.checkbox(&mut self.draft.show_capture_dialog, "Ask before starting downloads sent by the browser");
        ui.checkbox(&mut self.draft.clipboard_monitor, "Watch the clipboard for download links");
        ui.checkbox(&mut self.draft.notify_on_complete, "Notify me when a download finishes");
        ui.add_enabled_ui(self.tray_available, |ui| {
            ui.checkbox(&mut self.ui_draft.minimize_to_tray, "Hide to the notification area when minimised");
        });

        section(ui, "Appearance");
        ui.horizontal(|ui| {
            for t in [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark] {
                ui.radio_value(&mut self.ui_draft.theme, t, t.label());
            }
        });

        section(ui, "Updates");
        ui.checkbox(&mut self.draft.check_for_updates, "Check for a new version of Spate once a day");
        ui.weak("Spate reads the version number on GitHub and asks before installing anything. Help > Check for updates looks right away.");
    }

    fn connection(&mut self, ui: &mut egui::Ui) {
        let d = &mut self.draft;
        section(ui, "Per download");
        egui::Grid::new("conn_grid").num_columns(2).spacing(vec2(14.0, 10.0)).show(ui, |ui| {
            ui.label("Connections per download");
            ui.add(egui::Slider::new(&mut d.connections, 1..=32));
            ui.end_row();
            ui.label("Smallest part to split");
            ui.add(egui::DragValue::new(&mut d.min_split_kib).range(64..=1024 * 1024).speed(16).suffix(" KB"));
            ui.end_row();
            ui.label("Retries per connection");
            ui.add(egui::DragValue::new(&mut d.max_retries).range(0..=100));
            ui.end_row();
        });
        section(ui, "Queue and bandwidth");
        egui::Grid::new("queue_grid").num_columns(2).spacing(vec2(14.0, 10.0)).show(ui, |ui| {
            ui.label("Downloads at the same time");
            ui.add(egui::Slider::new(&mut d.max_parallel, 1..=20));
            ui.end_row();
            ui.label("Overall speed limit");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut d.speed_limit_kib).range(0..=10_000_000).speed(32).suffix(" KB/s"));
                ui.weak(if d.speed_limit_kib == 0 { "0 = unlimited" } else { "" });
            });
            ui.end_row();
        });
        section(ui, "Network");
        egui::Grid::new("net_grid").num_columns(2).spacing(vec2(14.0, 10.0)).show(ui, |ui| {
            ui.label("Proxy");
            ui.add(egui::TextEdit::singleline(&mut self.proxy).hint_text("empty = system settings, e.g. socks5://127.0.0.1:1080").desired_width(380.0));
            ui.end_row();
            ui.label("User agent");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut d.net.user_agent).desired_width(300.0));
                if ui.small_button("Default").clicked() {
                    d.net.user_agent = spate_core::http::DEFAULT_USER_AGENT.to_string();
                }
            });
            ui.end_row();
            ui.label("Connect timeout");
            ui.add(egui::DragValue::new(&mut d.net.connect_timeout_secs).range(1..=300).suffix(" s"));
            ui.end_row();
            ui.label("Restart a stalled connection after");
            ui.add(egui::DragValue::new(&mut d.net.read_timeout_secs).range(5..=600).suffix(" s"));
            ui.end_row();
            ui.label("Maximum redirects");
            ui.add(egui::DragValue::new(&mut d.net.max_redirects).range(0..=50));
            ui.end_row();
        });
    }

    fn scheduler(&mut self, ui: &mut egui::Ui) {
        let s: &mut Schedule = &mut self.draft.schedule;
        section(ui, "Daily schedule");
        ui.checkbox(&mut s.enabled, "Run the queue automatically every day");
        ui.add_enabled_ui(s.enabled, |ui| {
            egui::Grid::new("sched_grid").num_columns(2).spacing(vec2(14.0, 10.0)).show(ui, |ui| {
                ui.label("Start at");
                time_edit(ui, "start", &mut s.start);
                ui.end_row();
                let mut has_stop = s.stop.is_some();
                ui.checkbox(&mut has_stop, "Stop at");
                if has_stop {
                    let mut stop = s.stop.unwrap_or((7, 0));
                    time_edit(ui, "stop", &mut stop);
                    s.stop = Some(stop);
                } else {
                    s.stop = None;
                    ui.weak("runs until the queue is empty");
                }
                ui.end_row();
            });
        });
        ui.add_space(8.0);
        ui.weak("The queue starts the downloads you added with “Add to queue” / “Later”, a few at a time (see Connection > Downloads at the same time). Spate must be running, for example in the background, at the scheduled time.");
        ui.add_space(4.0);
        ui.weak("Use Queue > When all downloads finish to exit Spate or shut down the computer afterwards.");
    }

    fn browser(&mut self, ui: &mut egui::Ui, host_requested: &mut bool) {
        section(ui, "1. Register the browser helper");
        ui.label("The extension talks to Spate through a small helper that browsers must know about. Registering it is per user and needs no administrator rights.");
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.host_running, primary_button("Register helper")).clicked() {
                *host_requested = true;
                self.host_running = true;
                self.host_output = None;
            }
            if self.host_running {
                ui.spinner();
                ui.weak("Running spate-cli install-host…");
            }
        });
        if let Some(out) = &self.host_output {
            let (color, text) = match out {
                Ok(t) => (theme::GOOD, t.as_str()),
                Err(t) => (theme::BAD, t.as_str()),
            };
            egui::Frame::group(ui.style()).fill(ui.visuals().extreme_bg_color).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.colored_label(color, if out.is_ok() { "Helper registered" } else { "Registration failed" });
                egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(text).monospace()).wrap());
                });
            });
        }

        section(ui, "2. Load the extension");
        match sys::extension_dir() {
            Some(dir) => {
                ui.horizontal(|ui| {
                    ui.label("Extension folder:");
                    ui.add(egui::Label::new(RichText::new(dir.display().to_string()).monospace()).truncate());
                });
                ui.horizontal(|ui| {
                    if ui.button("Open folder").clicked() {
                        let _ = sys::open_path(&dir);
                    }
                    if ui.button("Copy path").clicked() {
                        ui.ctx().copy_text(dir.display().to_string());
                    }
                });
            }
            None => {
                ui.colored_label(theme::WARN, "The extension folder was not found next to Spate.");
            }
        }
        ui.add_space(4.0);
        ui.label(RichText::new("Chrome, Edge, Brave and other Chromium browsers").strong());
        ui.label("Open chrome://extensions (edge://extensions in Edge), switch on Developer mode, click “Load unpacked” and pick the extension folder above.");
        ui.add_space(4.0);
        ui.label(RichText::new("Firefox").strong());
        let firefox = sys::firefox_manifest();
        ui.label("Open about:debugging#/runtime/this-firefox, choose “Load Temporary Add-on…” and select the Firefox manifest:");
        match firefox {
            Some(p) => {
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(RichText::new(p.display().to_string()).monospace()).truncate());
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(p.display().to_string());
                    }
                });
            }
            None => {
                ui.weak("dist/firefox/manifest.json inside the extension folder (create it with `node build.mjs` there).");
            }
        }
        ui.weak("Temporary add-ons are removed when Firefox restarts; a signed build is needed to keep it installed.");
        ui.add_space(4.0);
        ui.weak("After both steps, downloads started in the browser are handed to Spate. The confirmation dialog can be turned off in General.");
    }
}

fn section(ui: &mut egui::Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).strong().color(ui.visuals().strong_text_color()));
    ui.add_space(2.0);
}

fn time_edit(ui: &mut egui::Ui, id: &str, t: &mut (u8, u8)) {
    ui.push_id(id, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.add(egui::DragValue::new(&mut t.0).range(0..=23).custom_formatter(|v, _| format!("{:02}", v as u8)));
            ui.label(":");
            ui.add(egui::DragValue::new(&mut t.1).range(0..=59).custom_formatter(|v, _| format!("{:02}", v as u8)));
        });
    });
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

const UPDATE_STOPPED: &str = "The update stopped unexpectedly.";

enum UpdateStage {
    Checking(Receiver<Result<Check, String>>),
    UpToDate,
    /// Main is at this newer version, whose download is not published yet.
    Unreleased(String),
    CheckFailed(String),
    Offer {
        release: Release,
        /// The folder to install into, or why this copy cannot update itself.
        target: Result<PathBuf, String>,
    },
    Installing { release: Release, rx: Receiver<Progress>, done: u64, total: Option<u64>, unpacking: bool },
    Failed { release: Release, error: String },
}

fn offer_stage(release: Release) -> UpdateStage {
    UpdateStage::Offer { target: update::install_dir(), release }
}

/// What the user decided in an [`UpdateDialog`].
pub enum UpdateOutcome {
    /// Do not offer this version again.
    Skip(String),
    /// The update is in place; restart into this executable.
    Restart(PathBuf),
}

enum UpdateAction {
    Install,
    Retry,
}

pub struct UpdateDialog {
    stage: UpdateStage,
    net: NetConfig,
    cancel: Arc<AtomicBool>,
    gate: Option<ConfirmGate>,
}

impl UpdateDialog {
    /// Check now and show the answer (Help > Check for updates).
    pub fn check(net: NetConfig, ctx: &egui::Context) -> Self {
        let rx = update::spawn_check(net.clone(), ctx.clone());
        Self { stage: UpdateStage::Checking(rx), net, cancel: Arc::default(), gate: None }
    }

    /// Offer `release`, which the daily check found. Like other dialogs that
    /// open by themselves, its buttons wait a moment before they work.
    pub fn offer(release: Release, net: NetConfig) -> Self {
        Self { stage: offer_stage(release), net, cancel: Arc::default(), gate: Some(ConfirmGate::default()) }
    }

    /// Take in what the check or the install reported. The executable to
    /// restart into once the update is in place.
    fn poll(&mut self) -> Option<PathBuf> {
        let next = match &mut self.stage {
            UpdateStage::Checking(rx) => match rx.try_recv() {
                Err(TryRecvError::Empty) => None,
                Ok(Ok(Check::UpToDate)) => Some(UpdateStage::UpToDate),
                Ok(Ok(Check::Unreleased(version))) => Some(UpdateStage::Unreleased(version)),
                Ok(Ok(Check::Available(release))) => Some(offer_stage(release)),
                Ok(Err(e)) => Some(UpdateStage::CheckFailed(e)),
                Err(TryRecvError::Disconnected) => Some(UpdateStage::CheckFailed(UPDATE_STOPPED.into())),
            },
            UpdateStage::Installing { release, rx, done, total, unpacking } => loop {
                match rx.try_recv() {
                    Ok(Progress::Downloading { done: d, total: t }) => {
                        *done = d;
                        *total = t;
                    }
                    Ok(Progress::Installing) => *unpacking = true,
                    Ok(Progress::Finished(Ok(exe))) => return Some(exe),
                    Ok(Progress::Finished(Err(error))) => break Some(UpdateStage::Failed { release: release.clone(), error }),
                    Err(TryRecvError::Empty) => break None,
                    Err(TryRecvError::Disconnected) => break Some(UpdateStage::Failed { release: release.clone(), error: UPDATE_STOPPED.into() }),
                }
            },
            _ => None,
        };
        if let Some(stage) = next {
            self.stage = stage;
        }
        None
    }

    /// `active` is the number of running downloads, which the restart pauses.
    pub fn show(&mut self, ctx: &egui::Context, active: usize) -> DialogResult<UpdateOutcome> {
        if let Some(exe) = self.poll() {
            return DialogResult::Done(UpdateOutcome::Restart(exe));
        }
        let armed = gate_open(&mut self.gate, ctx);
        let mut result = DialogResult::Open;
        let mut action = None;
        let modal = egui::Modal::new(Id::unique("update_dialog")).frame(dialog_frame(ctx)).show(ctx, |ui| {
            ui.set_width(480.0);
            // Button rows are one row high, so the dialog shrinks when a
            // stage with less to show follows a taller one.
            let buttons = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
                ui.add_space(10.0);
                ui.horizontal(|ui| ui.with_layout(Layout::right_to_left(Align::Center), |ui| add(ui)));
            };
            let close = |ui: &mut egui::Ui, result: &mut DialogResult<UpdateOutcome>| {
                buttons(ui, &mut |ui| {
                    if ui.add(primary_button("Close")).clicked() {
                        *result = DialogResult::Cancel;
                    }
                });
            };
            match &self.stage {
                UpdateStage::Checking(_) => {
                    title(ui, "Checking for updates");
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Looking up the latest version on GitHub…");
                    });
                    close(ui, &mut result);
                }
                UpdateStage::UpToDate => {
                    title(ui, "Spate is up to date");
                    ui.label(format!("Version {} is the latest.", update::CURRENT));
                    close(ui, &mut result);
                }
                UpdateStage::Unreleased(version) => {
                    title(ui, "Spate is up to date");
                    ui.label(format!("You have version {}. Version {version} is on its way and is offered here once its download is published.", update::CURRENT));
                    close(ui, &mut result);
                }
                UpdateStage::CheckFailed(error) => {
                    title(ui, "Could not check for updates");
                    ui.colored_label(theme::BAD, error);
                    close(ui, &mut result);
                }
                UpdateStage::Offer { release, target } => {
                    title(ui, &format!("Spate {} is available", release.version));
                    ui.weak(format!("You have version {}.", update::CURRENT));
                    release_notes(ui, &release.notes);
                    ui.add_space(6.0);
                    match target {
                        Ok(_) if active == 1 => {
                            ui.colored_label(theme::WARN, "The running download pauses while Spate restarts, then continues.");
                        }
                        Ok(_) if active > 1 => {
                            ui.colored_label(theme::WARN, format!("The {active} running downloads pause while Spate restarts, then continue."));
                        }
                        Ok(_) => {}
                        Err(why) => {
                            ui.weak(format!("{why} Download the new version from its release page instead."));
                        }
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        let skip = ui.add_enabled(armed, secondary_button("Skip this version")).on_disabled_hover_text(WAIT_HINT);
                        if skip.clicked() {
                            result = DialogResult::Done(UpdateOutcome::Skip(release.version.clone()));
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if target.is_ok() {
                                if ui.add_enabled(armed, primary_button("Update now")).on_disabled_hover_text(WAIT_HINT).clicked() {
                                    action = Some(UpdateAction::Install);
                                }
                            } else if ui.add(primary_button("Open download page")).clicked() {
                                ui.ctx().open_url(egui::OpenUrl::new_tab(update::release_page(&release.version)));
                                result = DialogResult::Cancel;
                            }
                            if ui.add(secondary_button("Later")).clicked() {
                                result = DialogResult::Cancel;
                            }
                        });
                    });
                }
                UpdateStage::Installing { release, done, total, unpacking, .. } => {
                    title(ui, &format!("Updating to Spate {}", release.version));
                    let bar = if *unpacking {
                        egui::ProgressBar::new(1.0).text("Installing…")
                    } else {
                        let fraction = total.filter(|t| *t > 0).map_or(0.0, |t| *done as f32 / t as f32);
                        let text = match total {
                            Some(t) => format!("Downloading {} of {}", format::size(*done), format::size(*t)),
                            None => format!("Downloading {}", format::size(*done)),
                        };
                        egui::ProgressBar::new(fraction).text(text)
                    };
                    ui.add(bar);
                    ui.weak("Spate restarts once the new version is in place.");
                    buttons(ui, &mut |ui| {
                        if ui.add_enabled(!*unpacking, secondary_button("Cancel")).clicked() {
                            self.cancel.store(true, Ordering::Relaxed);
                            result = DialogResult::Cancel;
                        }
                    });
                }
                UpdateStage::Failed { release, error } => {
                    title(ui, "The update did not finish");
                    ui.colored_label(theme::BAD, error);
                    ui.label(format!("Spate {} is still installed.", update::CURRENT));
                    buttons(ui, &mut |ui| {
                        if ui.add(primary_button("Try again")).clicked() {
                            action = Some(UpdateAction::Retry);
                        }
                        if ui.add(secondary_button("Open download page")).clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(update::release_page(&release.version)));
                            result = DialogResult::Cancel;
                        }
                        if ui.add(secondary_button("Close")).clicked() {
                            result = DialogResult::Cancel;
                        }
                    });
                }
            }
        });

        let next = match (action, &self.stage) {
            (Some(UpdateAction::Install), UpdateStage::Offer { release, target: Ok(dir) }) => {
                self.cancel = Arc::default();
                let rx = update::spawn_install(release.clone(), dir.clone(), self.net.clone(), self.cancel.clone(), ctx.clone());
                Some(UpdateStage::Installing { release: release.clone(), rx, done: 0, total: None, unpacking: false })
            }
            (Some(UpdateAction::Retry), UpdateStage::Failed { release, .. }) => Some(offer_stage(release.clone())),
            _ => None,
        };
        if let Some(stage) = next {
            self.stage = stage;
        }
        let installing = matches!(self.stage, UpdateStage::Installing { .. });
        if modal.should_close() && matches!(result, DialogResult::Open) && !installing && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = DialogResult::Cancel;
        }
        result
    }
}

/// A release's section of the changelog, lightly formatted.
fn release_notes(ui: &mut egui::Ui, notes: &str) {
    if notes.is_empty() {
        return;
    }
    ui.add_space(8.0);
    ui.label(RichText::new("What's new").strong());
    egui::Frame::group(ui.style()).fill(ui.visuals().extreme_bg_color).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
            for line in notes.lines() {
                let line = line.trim().replace(['`', '*'], "");
                if let Some(heading) = line.strip_prefix("### ") {
                    ui.add_space(4.0);
                    ui.label(RichText::new(heading).strong());
                } else if let Some(item) = line.strip_prefix("- ") {
                    ui.horizontal_top(|ui| {
                        ui.label("•");
                        ui.add(egui::Label::new(item).wrap());
                    });
                } else if !line.is_empty() {
                    ui.add(egui::Label::new(line).wrap());
                }
            }
        });
    });
}

pub fn about_contents(ui: &mut egui::Ui) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(56.0, 56.0), egui::Sense::hover());
        crate::app::paint_logo(ui.painter(), rect);
        ui.vertical(|ui| {
            ui.label(RichText::new("Spate").heading().strong());
            ui.label(format!("Version {}", update::CURRENT));
            ui.weak("Open-source download manager");
        });
    });
    ui.add_space(10.0);
    ui.label("Splits each download into parts fetched over several connections at once, keeps every connection busy until the end, resumes interrupted transfers and takes over downloads from your browser.");
    ui.add_space(6.0);
    ui.label("Licensed under MIT or Apache-2.0.");
    ui.horizontal(|ui| {
        ui.label("Data folder:");
        let dir = spate_core::paths::data_dir();
        if ui.link(dir.display().to_string()).clicked() {
            let _ = sys::open_path(&dir);
        }
    });
    ui.hyperlink_to("Project page", update::PROJECT_URL);
}

pub struct AboutDialog;

impl AboutDialog {
    pub fn show(ctx: &egui::Context) -> bool {
        let mut close = false;
        let modal = egui::Modal::new(Id::unique("about_dialog")).frame(dialog_frame(ctx)).show(ctx, |ui| {
            ui.set_width(440.0);
            about_contents(ui);
            ui.add_space(10.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(primary_button("Close")).clicked() {
                    close = true;
                }
            });
        });
        close || modal.should_close()
    }
}

pub fn shortcuts_dialog(ctx: &egui::Context) -> bool {
    let mut close = false;
    let modal = egui::Modal::new(Id::unique("shortcuts_dialog")).frame(dialog_frame(ctx)).show(ctx, |ui| {
        ui.set_width(380.0);
        title(ui, "Keyboard shortcuts");
        egui::Grid::new("keys").num_columns(2).spacing(vec2(24.0, 6.0)).striped(true).show(ui, |ui| {
            for (k, what) in [
                ("Ctrl+N", "Add a download"),
                ("Ctrl+B", "Add several downloads"),
                ("Ctrl+V", "Add links from the clipboard (list focused)"),
                ("Ctrl+A", "Select all"),
                ("Ctrl+F", "Search the list"),
                ("Enter", "Open the selected file"),
                ("Delete", "Delete the selection"),
                ("Space", "Pause / resume the selection"),
                ("Esc", "Close a dialog"),
            ] {
                ui.monospace(k);
                ui.label(what);
                ui.end_row();
            }
        });
        ui.add_space(10.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.add(primary_button("Close")).clicked() {
                close = true;
            }
        });
    });
    close || modal.should_close()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enter(pressed: bool, repeat: bool) -> egui::Event {
        egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed, repeat, modifiers: egui::Modifiers::NONE }
    }

    fn header(name: &str, value: &str) -> (String, String) {
        (name.into(), value.into())
    }

    fn capture(url: &str, name: Option<&str>, cookies: Option<&str>) -> AddItem {
        AddItem {
            url: url.into(),
            file_name: name.map(String::from),
            cookies: cookies.map(String::from),
            referrer: Some("https://site.example/".into()),
            ..Default::default()
        }
    }

    #[test]
    fn confirm_gate_waits_for_a_focused_second() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut gate = ConfirmGate::default();
        assert_eq!(gate.remaining(t0), ConfirmGate::DELAY);
        assert!(!gate.update(t0, true));
        assert!(!gate.update(ms(999), true));
        assert_eq!(gate.remaining(ms(900)), Duration::from_millis(100));
        assert!(gate.update(ms(1000), true));
        // Switching away and back starts the wait over.
        assert!(!gate.update(ms(1100), false));
        assert!(!gate.update(ms(1200), true));
        assert!(!gate.update(ms(2150), true));
        assert!(gate.update(ms(2200), true));
        // A dialog waiting in a window without focus never opens.
        let mut gate = ConfirmGate::default();
        assert!((0..50).all(|i| !gate.update(ms(i * 100), false)));
    }

    #[test]
    fn held_enter_does_not_confirm() {
        assert!(enter_pressed(&[enter(true, false)]));
        assert!(!enter_pressed(&[enter(true, true), enter(true, true)]));
        assert!(!enter_pressed(&[enter(false, false)]));
        assert!(!enter_pressed(&[egui::Event::Text("\n".into())]));
        assert!(enter_pressed(&[enter(true, true), enter(false, false), enter(true, false)]));
    }

    #[test]
    fn dialogs_that_open_by_themselves_are_gated() {
        let s = Settings::default();
        let url = "https://site.example/files/a.zip".to_string();
        assert!(AddDialog::from_capture(&capture(&url, None, None), &s).gate.is_some());
        assert!(AddDialog::new(url.clone(), Source::ClipboardWatch, &s).gate.is_some());
        assert!(AddDialog::new(url.clone(), Source::Clipboard, &s).gate.is_none());
        assert!(AddDialog::new(String::new(), Source::Typed, &s).gate.is_none());
        assert!(BatchDialog::new(vec![capture(&url, None, None)], false, &s).gate.is_some());
        assert!(BatchDialog::from_urls(vec![url], &s).gate.is_none());
    }

    #[test]
    fn checks_before_confirming_never_carry_credentials() {
        let creds = [header("Cookie", "s=1"), header("Referer", "https://site.example/")];
        let plain = [header("Referer", "https://site.example/"), header("User-Agent", "UA")];
        assert!(checks_on_its_own(Source::Typed, &[]));
        assert!(!checks_on_its_own(Source::Clipboard, &[]));
        assert!(!checks_on_its_own(Source::ClipboardWatch, &[]));
        assert!(!checks_on_its_own(Source::Capture, &creds));
        assert!(!checks_on_its_own(Source::Capture, &[header("authorization", "Bearer x")]));
        assert!(checks_on_its_own(Source::Capture, &plain));
        let all = [header("Cookie", "s=1"), header("Authorization", "Basic x"), header("Referer", "r"), header("User-Agent", "UA")];
        assert_eq!(check_headers(&all), vec![header("Referer", "r"), header("User-Agent", "UA")]);

        let s = Settings::default();
        let url = "https://site.example/files/a.zip";
        let scheduled = |d: &AddDialog| d.url_changed.is_some();
        assert!(!scheduled(&AddDialog::from_capture(&capture(url, Some("a.zip"), Some("s=1")), &s)));
        assert!(scheduled(&AddDialog::from_capture(&capture(url, Some("a.zip"), None), &s)));
        assert!(!scheduled(&AddDialog::new(url.into(), Source::Clipboard, &s)));
        assert!(!scheduled(&AddDialog::new(url.into(), Source::ClipboardWatch, &s)));
        // Asking for it (the Check button) is what starts a check.
        let mut d = AddDialog::new(url.into(), Source::Clipboard, &s);
        d.check_now();
        assert!(scheduled(&d));
    }

    #[test]
    fn the_name_shown_is_the_name_requested() {
        let s = Settings::default();
        // Equal to the name in the address: still requested, so the server
        // cannot swap it.
        let d = AddDialog::from_capture(&capture("https://site.example/invoice.pdf", Some("invoice.pdf"), Some("s=1")), &s);
        let req = d.request(StartMode::Now);
        assert_eq!(req.file_name.as_deref(), Some("invoice.pdf"));
        assert!(req.headers.contains(&header("Cookie", "s=1")), "the download itself keeps the cookies");
        let d = AddDialog::new("https://site.example/files/setup.msi".into(), Source::Clipboard, &s);
        assert_eq!(d.request(StartMode::Queue).file_name.as_deref(), Some("setup.msi"));
        // A name that would read differently is shown the way it is saved.
        let d = AddDialog::from_capture(&capture("https://site.example/x", Some("invoice\u{202E}fdp.exe"), None), &s);
        assert_eq!(d.file_name, "invoicefdp.exe");

        let items = vec![
            capture("https://site.example/files/a.zip", None, None),
            capture("https://site.example/get?id=1", Some("report  .pdf"), None),
        ];
        let out = BatchDialog::new(items, false, &s).outcome(StartMode::Now);
        let names: Vec<_> = out.requests.iter().map(|r| r.file_name.clone().unwrap_or_default()).collect();
        assert_eq!(names, ["a.zip", "report .pdf"]);
    }

    #[test]
    fn hls_is_recognised_from_the_browser_type() {
        let item = |kind: Option<&str>, mime: Option<&str>| AddItem {
            url: "https://cdn.example/stream".into(),
            kind: kind.map(String::from),
            mime: mime.map(String::from),
            ..Default::default()
        };
        assert_eq!(kind_from_item(&item(Some("HLS"), None)), Some(DownloadKind::Hls));
        assert_eq!(kind_from_item(&item(None, Some("application/vnd.apple.mpegurl; charset=utf-8"))), Some(DownloadKind::Hls));
        assert_eq!(kind_from_item(&item(Some("http"), Some("application/x-mpegURL"))), Some(DownloadKind::Http));
        assert_eq!(kind_from_item(&item(None, Some("video/mp4"))), None);
    }
}
