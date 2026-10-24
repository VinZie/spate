//! Clipboard monitor: polls the clipboard on a background thread and
//! reports newly copied download links.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use crate::app::UiEvent;
use crate::urls;

const POLL: Duration = Duration::from_millis(1500);

pub struct ClipWatcher {
    shared: Arc<Shared>,
}

struct Shared {
    enabled: AtomicBool,
    /// Re-read the current contents without reporting them (set when the
    /// monitor is switched on, so stale clipboard text is ignored).
    prime: AtomicBool,
    last: Mutex<String>,
}

impl ClipWatcher {
    pub fn start(tx: Sender<UiEvent>, ctx: egui::Context) -> Self {
        let shared = Arc::new(Shared { enabled: AtomicBool::new(false), prime: AtomicBool::new(true), last: Mutex::new(String::new()) });
        let s = shared.clone();
        let spawned = std::thread::Builder::new().name("spate-clipboard".into()).spawn(move || {
            let mut clipboard: Option<arboard::Clipboard> = None;
            loop {
                std::thread::sleep(POLL);
                if !s.enabled.load(Ordering::Relaxed) {
                    clipboard = None;
                    continue;
                }
                if clipboard.is_none() {
                    clipboard = arboard::Clipboard::new().ok();
                }
                let Some(cb) = clipboard.as_mut() else { continue };
                let text = match cb.get_text() {
                    Ok(t) => t,
                    Err(arboard::Error::ContentNotAvailable) => String::new(),
                    Err(_) => {
                        clipboard = None;
                        continue;
                    }
                };
                let mut last = s.last.lock().unwrap_or_else(|e| e.into_inner());
                if s.prime.swap(false, Ordering::Relaxed) {
                    *last = text;
                    continue;
                }
                if *last == text {
                    continue;
                }
                *last = text.clone();
                drop(last);
                if let Some(url) = urls::clipboard_download(&text) {
                    if tx.send(UiEvent::ClipboardUrl(url)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            }
        });
        if let Err(e) = spawned {
            eprintln!("clipboard monitor unavailable: {e}");
        }
        Self { shared }
    }

    pub fn set_enabled(&self, on: bool) {
        let was = self.shared.enabled.swap(on, Ordering::Relaxed);
        if on && !was {
            self.shared.prime.store(true, Ordering::Relaxed);
        }
    }

    /// Text Spate itself put on the clipboard must not trigger a prompt.
    pub fn note_own_copy(&self, text: &str) {
        *self.shared.last.lock().unwrap_or_else(|e| e.into_inner()) = text.to_string();
    }
}
