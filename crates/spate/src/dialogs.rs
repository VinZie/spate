//! Modal dialogs: add URL / captured download, batch add, delete
//! confirmation, options and about.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Id, Layout, RichText, vec2};
use spate_core::ipc::AddItem;
use spate_core::settings::Schedule;
use spate_core::{AddRequest, Category, DownloadKind, ItemView, Manager, ProbeInfo, Settings, StartMode};

use crate::theme::{self, ThemeChoice};
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

fn kind_from_item(kind: Option<&str>) -> Option<DownloadKind> {
    match kind.map(str::to_ascii_lowercase).as_deref() {
        Some("hls") => Some(DownloadKind::Hls),
        Some("http") => Some(DownloadKind::Http),
        _ => None,
    }
}

/// Request for a browser/CLI item that is added without asking.
pub fn request_from_item(item: &AddItem, start: StartMode) -> AddRequest {
    AddRequest {
        url: item.url.trim().to_string(),
        headers: item.request_headers(),
        file_name: item.file_name.clone().filter(|n| !n.trim().is_empty()),
        dir: item.dir.clone(),
        start,
        kind: kind_from_item(item.kind.as_deref()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Add URL / captured download
// ---------------------------------------------------------------------------

enum Probe {
    Idle,
    Pending(tokio::sync::oneshot::Receiver<spate_core::Result<ProbeInfo>>),
    Done(ProbeInfo),
    Failed(String),
}

pub struct AddDialog {
    id: Id,
    /// Opened for a download captured from the browser.
    pub capture: bool,
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
    kind: Option<DownloadKind>,
    probe: Probe,
    probed_url: String,
    url_changed: Option<Instant>,
    focus_url: bool,
    error: Option<String>,
}

pub enum DialogResult<T> {
    Open,
    Cancel,
    Done(T),
}

impl AddDialog {
    pub fn new(url: String, settings: &Settings) -> Self {
        let name = if url.is_empty() { String::new() } else { urls::file_name_from_url(&url) };
        let dir = settings.dir_for(Category::for_file_name(&name)).display().to_string();
        let has_url = urls::is_http_url(&url);
        Self {
            id: Id::unique(("add_dialog", Instant::now())),
            capture: false,
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
            kind: None,
            probe: Probe::Idle,
            probed_url: String::new(),
            // Probe a prefilled URL right away.
            url_changed: has_url.then(|| Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now)),
            focus_url: !has_url,
            error: None,
        }
    }

    pub fn from_capture(item: &AddItem, settings: &Settings) -> Self {
        let mut d = Self::new(item.url.trim().to_string(), settings);
        d.capture = true;
        d.headers = item.request_headers();
        if let Some(name) = item.file_name.as_deref().filter(|n| !n.trim().is_empty()) {
            d.file_name = name.trim().to_string();
            d.name_edited = true;
            d.dir = settings.dir_for(Category::for_file_name(&d.file_name)).display().to_string();
        }
        if let Some(dir) = &item.dir {
            d.dir = dir.display().to_string();
            d.dir_edited = true;
        }
        d.browser_size = item.size;
        d.kind = kind_from_item(item.kind.as_deref());
        d.focus_url = false;
        d
    }

    fn poll_probe(&mut self, manager: &Manager, settings: &Settings, ctx: &egui::Context) {
        let url = self.url.trim().to_string();
        if let Some(t) = self.url_changed {
            if t.elapsed() >= Duration::from_millis(450) && urls::is_http_url(&url) && url != self.probed_url {
                self.url_changed = None;
                self.probed_url = url.clone();
                self.probe = Probe::Pending(manager.probe(url, self.headers.clone()));
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
        AddRequest {
            url: self.url.trim().to_string(),
            headers: self.headers.clone(),
            file_name: (self.name_edited || probe_named).then(|| self.file_name.trim().to_string()).filter(|n| !n.is_empty()),
            dir: (self.dir_edited || probe_named || self.capture).then(|| PathBuf::from(self.dir.trim())).filter(|d| !d.as_os_str().is_empty()),
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
        let mut result = DialogResult::Open;
        let modal = egui::Modal::new(self.id).frame(dialog_frame(ctx)).show(ctx, |ui| {
            ui.set_width(560.0);
            let heading = if self.capture { "Confirm download" } else { "Add download" };
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
                        self.file_name = urls::file_name_from_url(self.url.trim());
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
                let (now_label, later_label) = if self.capture { ("Start", "Later") } else { ("Start now", "Add to queue") };
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(secondary_button("Cancel")).clicked() {
                        result = DialogResult::Cancel;
                    }
                    if ui.add(secondary_button(later_label)).on_hover_text("Add to the queue; it starts when the queue runs").clicked() {
                        match self.validate() {
                            Ok(()) => result = DialogResult::Done(self.request(StartMode::Queue)),
                            Err(e) => self.error = Some(e),
                        }
                    }
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.add(primary_button(now_label)).clicked() || enter {
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

    fn info_card(&self, ui: &mut egui::Ui, items: &[ItemView]) {
        egui::Frame::group(ui.style()).fill(ui.visuals().faint_bg_color).corner_radius(8).inner_margin(10).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let cat = Category::for_file_name(&self.file_name);
                icons::category_glyph(ui, cat, 30.0);
                ui.vertical(|ui| {
                    let name = if self.file_name.trim().is_empty() { "–" } else { self.file_name.trim() };
                    ui.label(RichText::new(name).strong());
                    let line = match &self.probe {
                        Probe::Idle if !urls::is_http_url(self.url.trim()) => RichText::new("Paste or type a download address.").weak(),
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
                    ui.label(line);
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
}

pub struct BatchOutcome {
    pub requests: Vec<AddRequest>,
}

impl BatchDialog {
    pub fn new(items: Vec<AddItem>, editable: bool, settings: &Settings) -> Self {
        Self {
            id: Id::unique(("batch_dialog", Instant::now())),
            entries: items.into_iter().map(|item| BatchEntry { checked: true, item }).collect(),
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
                            let name = e.item.file_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| urls::file_name_from_url(&e.item.url));
                            icons::category_glyph(ui, Category::for_file_name(&name), 16.0);
                            ui.add(egui::Label::new(RichText::new(&name).strong()).truncate()).on_hover_text(&e.item.url);
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
                if ui.add_enabled(any, secondary_button("Add to queue")).clicked() {
                    result = DialogResult::Done(self.outcome(StartMode::Queue));
                }
                if ui.add_enabled(any, primary_button("Start all")).clicked() {
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
                ui.label(format!("Remove “{}” from the list?", self.names.first().map(String::as_str).unwrap_or("")));
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
                if ui.add(btn).clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
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

pub fn about_contents(ui: &mut egui::Ui) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(56.0, 56.0), egui::Sense::hover());
        crate::app::paint_logo(ui.painter(), rect);
        ui.vertical(|ui| {
            ui.label(RichText::new("Spate").heading().strong());
            ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
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
    ui.hyperlink_to("Project page", "https://github.com/spate/spate");
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
