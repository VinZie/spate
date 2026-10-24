//! Native-messaging host for the browser extension.
//!
//! Browsers start the host with the caller's identity on the command line:
//! Chromium-based browsers pass `chrome-extension://<id>/` (plus
//! `--parent-window=<hwnd>` on Windows), Firefox passes the manifest path and
//! the extension ID. Messages are length-prefixed JSON on stdin/stdout, so in
//! this mode nothing but replies may ever be written to stdout; diagnostics go
//! to stderr and `native-host.log` in the data folder.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Args;
use spate_core::ipc::{self, AddItem, NativeRequest, NativeResponse};
use spate_core::paths;
use serde::Serialize;

use crate::{EXIT_ERROR, EXIT_OK, app, install};

const LOG_FILE: &str = "native-host.log";
const LOG_LIMIT: u64 = 512 * 1024;

#[derive(Args, Debug)]
pub struct HostArgs {
    /// Caller to authorize: `chrome-extension://<id>/` or a Firefox add-on ID
    #[arg(long, value_name = "ORIGIN")]
    pub origin: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    Chromium { origin: String },
    Firefox { manifest: String, extension_id: String },
    /// `spate-cli native-host [--origin ...]`, typed by a person.
    Explicit { origin: Option<String> },
}

/// Recognise a browser launching us as a native-messaging host.
pub fn detect(argv: &[OsString]) -> Option<Launch> {
    let first = argv.get(1)?.to_str()?;
    if first.starts_with("chrome-extension://") {
        return Some(Launch::Chromium { origin: first.to_string() });
    }
    if first.to_ascii_lowercase().ends_with(".json")
        && let Some(id) = argv.get(2).and_then(|a| a.to_str())
        && looks_like_gecko_id(id)
    {
        return Some(Launch::Firefox { manifest: first.to_string(), extension_id: id.to_string() });
    }
    None
}

fn looks_like_gecko_id(s: &str) -> bool {
    let plain = !s.is_empty() && !s.contains(char::is_whitespace);
    plain && (s.contains('@') || (s.starts_with('{') && s.ends_with('}')))
}

/// Extension IDs allowed to use the host: the official ones plus any that
/// `install-host --extension-id` wrote into our manifests.
#[derive(Clone, Debug, Default)]
pub struct Allowed {
    pub chromium: Vec<String>,
    pub firefox: Vec<String>,
}

impl Allowed {
    pub fn load(data_dir: &Path) -> Self {
        let mut a = Allowed {
            chromium: vec![ipc::CHROMIUM_EXTENSION_ID.to_string()],
            firefox: vec![ipc::FIREFOX_EXTENSION_ID.to_string()],
        };
        let (chromium, firefox) = install::manifest_ids(data_dir);
        for id in chromium {
            if !a.chromium.contains(&id) {
                a.chromium.push(id);
            }
        }
        for id in firefox {
            if !a.firefox.contains(&id) {
                a.firefox.push(id);
            }
        }
        a
    }
}

pub fn authorize(launch: &Launch, allowed: &Allowed) -> Result<(), String> {
    let chromium_ok = |origin: &str| {
        let id = origin.trim().strip_prefix("chrome-extension://").map(|r| r.trim_end_matches('/'));
        id.is_some_and(|id| allowed.chromium.iter().any(|a| a == id))
    };
    let ok = match launch {
        Launch::Chromium { origin } => chromium_ok(origin),
        Launch::Firefox { extension_id, .. } => allowed.firefox.iter().any(|a| a == extension_id.trim()),
        Launch::Explicit { origin: None } => true,
        Launch::Explicit { origin: Some(o) } if o.starts_with("chrome-extension://") => chromium_ok(o),
        Launch::Explicit { origin: Some(o) } => allowed.firefox.iter().any(|a| a == o.trim()),
    };
    if ok { Ok(()) } else { Err("this extension is not allowed to use Spate".into()) }
}

/// What the host needs from the desktop app.
pub trait AppLink {
    fn is_running(&self) -> bool;
    fn add(&self, items: Vec<AddItem>, interactive: bool) -> Result<(), String>;
}

/// Reply to the extension: [`NativeResponse`] plus host-specific fields.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostReply {
    #[serde(flatten)]
    pub base: NativeResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_running: Option<bool>,
}

impl HostReply {
    fn ok(app_running: Option<bool>) -> Self {
        Self { base: NativeResponse { ok: true, error: None, version: ipc::VERSION.into() }, app_running }
    }

    fn err(msg: impl Into<String>) -> Self {
        Self { base: NativeResponse { ok: false, error: Some(msg.into()), version: ipc::VERSION.into() }, app_running: None }
    }
}

pub fn handle(msg: &[u8], app: &dyn AppLink, auth: &Result<(), String>) -> HostReply {
    if let Err(e) = auth {
        return HostReply::err(e.clone());
    }
    let request: NativeRequest = match serde_json::from_slice(msg) {
        Ok(r) => r,
        Err(e) => return HostReply::err(format!("bad message: {e}")),
    };
    match request {
        NativeRequest::Ping => HostReply::ok(Some(app.is_running())),
        NativeRequest::Download { items, interactive } => {
            let mut out: Vec<AddItem> = Vec::with_capacity(items.len());
            for item in items {
                let raw = item.url.trim();
                if raw.is_empty() {
                    continue;
                }
                match url::Url::parse(raw) {
                    Ok(u) if matches!(u.scheme(), "http" | "https") => {}
                    Ok(u) => return HostReply::err(format!("unsupported URL scheme `{}`", u.scheme())),
                    Err(e) => return HostReply::err(format!("invalid URL: {e}")),
                }
                out.push(item.into());
            }
            if out.is_empty() {
                return HostReply::err("no URLs to download");
            }
            match app.add(out, interactive) {
                Ok(()) => HostReply::ok(Some(true)),
                Err(e) => HostReply::err(e),
            }
        }
    }
}

/// Answer framed requests from `input` until it closes.
pub fn serve(input: &mut impl Read, output: &mut impl Write, app: &dyn AppLink, auth: &Result<(), String>) -> io::Result<()> {
    loop {
        let msg = match ipc::read_native_message(input) {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(()),
            Err(e) => {
                if e.kind() == io::ErrorKind::InvalidData {
                    let reply = serde_json::to_vec(&HostReply::err(e.to_string()))?;
                    let _ = ipc::write_native_message(output, &reply);
                }
                return Err(e);
            }
        };
        let reply = serde_json::to_vec(&handle(&msg, app, auth))?;
        ipc::write_native_message(output, &reply)?;
    }
}

/// Append-only diagnostics (stdout is reserved for the protocol).
pub struct HostLog {
    file: Mutex<Option<File>>,
}

impl HostLog {
    pub fn open(data_dir: &Path) -> Self {
        let path = data_dir.join(LOG_FILE);
        let _ = std::fs::create_dir_all(data_dir);
        let too_big = std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_LIMIT);
        let file = OpenOptions::new().create(true).write(true).append(!too_big).truncate(too_big).open(path).ok();
        Self { file: Mutex::new(file) }
    }

    pub fn line(&self, msg: &str) {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let text = format!("{ts} [{}] {msg}\n", std::process::id());
        let _ = io::stderr().write_all(text.as_bytes());
        if let Ok(mut f) = self.file.lock()
            && let Some(f) = f.as_mut()
        {
            let _ = f.write_all(text.as_bytes());
        }
    }
}

struct RealApp<'a> {
    data_dir: PathBuf,
    log: &'a HostLog,
}

impl AppLink for RealApp<'_> {
    fn is_running(&self) -> bool {
        ipc::is_running(&self.data_dir)
    }

    fn add(&self, items: Vec<AddItem>, interactive: bool) -> Result<(), String> {
        self.log.line(&format!("forwarding {} download(s), interactive={interactive}", items.len()));
        app::send_add(&self.data_dir, items, interactive, None, &mut |m| self.log.line(m))
    }
}

pub fn run(launch: Launch) -> u8 {
    // Anything we start (the app) must not inherit the browser's pipes, or
    // the browser would never see them close when we exit.
    #[cfg(windows)]
    make_std_handles_uninheritable();

    let data_dir = paths::data_dir();
    let log = HostLog::open(&data_dir);
    log.line(&format!("native host {} started: {launch:?}", ipc::VERSION));
    let auth = authorize(&launch, &Allowed::load(&data_dir));
    if let Err(e) = &auth {
        log.line(&format!("rejecting caller: {e}"));
    }
    let app = RealApp { data_dir, log: &log };
    let result = serve(&mut io::stdin().lock(), &mut io::stdout().lock(), &app, &auth);
    match result {
        Ok(()) => {
            log.line("input closed, exiting");
            EXIT_OK
        }
        Err(e) => {
            log.line(&format!("stopping: {e}"));
            EXIT_ERROR
        }
    }
}

#[cfg(windows)]
fn make_std_handles_uninheritable() {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: querying and flagging our own standard handles.
        unsafe {
            let h = GetStdHandle(id);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeApp {
        running: bool,
        fail: Option<String>,
        added: RefCell<Vec<(Vec<AddItem>, bool)>>,
    }

    impl AppLink for FakeApp {
        fn is_running(&self) -> bool {
            self.running
        }
        fn add(&self, items: Vec<AddItem>, interactive: bool) -> Result<(), String> {
            self.added.borrow_mut().push((items, interactive));
            self.fail.clone().map_or(Ok(()), Err)
        }
    }

    fn frames(msgs: &[&[u8]]) -> Vec<u8> {
        let mut buf = Vec::new();
        for m in msgs {
            ipc::write_native_message(&mut buf, m).unwrap();
        }
        buf
    }

    fn replies(mut out: &[u8]) -> Vec<serde_json::Value> {
        let mut v = Vec::new();
        while let Some(m) = ipc::read_native_message(&mut out).unwrap() {
            v.push(serde_json::from_slice(&m).unwrap());
        }
        v
    }

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn detects_browser_launches() {
        let id = ipc::CHROMIUM_EXTENSION_ID;
        let origin = format!("chrome-extension://{id}/");
        assert_eq!(
            detect(&os(&["cli.exe", &origin, "--parent-window=1234"])),
            Some(Launch::Chromium { origin: origin.clone() })
        );
        assert_eq!(
            detect(&os(&["cli.exe", "C:\\x\\com.spate.host.json", ipc::FIREFOX_EXTENSION_ID])),
            Some(Launch::Firefox { manifest: "C:\\x\\com.spate.host.json".into(), extension_id: ipc::FIREFOX_EXTENSION_ID.into() })
        );
        assert!(detect(&os(&["cli.exe", "a.json", "{8d3c-11}"])).is_some());
        assert_eq!(detect(&os(&["cli.exe", "get", "https://x/a.json"])), None);
        assert_eq!(detect(&os(&["cli.exe", "a.json", "not an id"])), None);
        assert_eq!(detect(&os(&["cli.exe"])), None);
    }

    #[test]
    fn authorizes_only_known_extensions() {
        let allowed = Allowed {
            chromium: vec![ipc::CHROMIUM_EXTENSION_ID.into(), "abcdefghijklmnopabcdefghijklmnop".into()],
            firefox: vec![ipc::FIREFOX_EXTENSION_ID.into()],
        };
        let ok = |l: Launch| authorize(&l, &allowed).is_ok();
        assert!(ok(Launch::Chromium { origin: format!("chrome-extension://{}/", ipc::CHROMIUM_EXTENSION_ID) }));
        assert!(ok(Launch::Chromium { origin: "chrome-extension://abcdefghijklmnopabcdefghijklmnop/".into() }));
        assert!(!ok(Launch::Chromium { origin: "chrome-extension://evilevilevilevilevilevilevilevil/".into() }));
        assert!(!ok(Launch::Chromium { origin: "chrome-extension://".into() }));
        assert!(ok(Launch::Firefox { manifest: "m.json".into(), extension_id: ipc::FIREFOX_EXTENSION_ID.into() }));
        assert!(!ok(Launch::Firefox { manifest: "m.json".into(), extension_id: "evil@example.com".into() }));
        assert!(ok(Launch::Explicit { origin: None }));
        assert!(!ok(Launch::Explicit { origin: Some("evil@example.com".into()) }));
    }

    #[test]
    fn answers_ping_and_download() {
        let app = FakeApp { running: true, ..Default::default() };
        let input = frames(&[
            br#"{"type":"ping"}"#,
            br#"{"type":"download","interactive":true,"items":[{"url":"https://example.com/a.zip","fileName":"a.zip","cookies":"s=1","referrer":"https://example.com/"}]}"#,
            b"not json",
            br#"{"type":"download","items":[{"url":"javascript:alert(1)"}]}"#,
            br#"{"type":"download","items":[]}"#,
        ]);
        let mut out = Vec::new();
        serve(&mut &input[..], &mut out, &app, &Ok(())).unwrap();
        let r = replies(&out);
        assert_eq!(r.len(), 5);
        assert_eq!(r[0]["ok"], true);
        assert_eq!(r[0]["appRunning"], true);
        assert_eq!(r[0]["version"], ipc::VERSION);
        assert!(r[0].get("error").is_none());
        assert_eq!(r[1]["ok"], true);
        assert_eq!(r[2]["ok"], false);
        assert!(r[2]["error"].as_str().unwrap().starts_with("bad message"));
        assert_eq!(r[3]["ok"], false);
        assert_eq!(r[4]["error"], "no URLs to download");

        let added = app.added.borrow();
        assert_eq!(added.len(), 1, "only the valid download reaches the app");
        let (items, interactive) = &added[0];
        assert!(*interactive);
        assert_eq!(items[0].file_name.as_deref(), Some("a.zip"));
        assert_eq!(items[0].cookies.as_deref(), Some("s=1"));
        assert_eq!(items[0].referrer.as_deref(), Some("https://example.com/"));
    }

    #[test]
    fn reports_app_errors_and_rejects_strangers() {
        let app = FakeApp { fail: Some("Spate did not respond".into()), ..Default::default() };
        let input = frames(&[br#"{"type":"download","items":[{"url":"https://example.com/a"}]}"#]);
        let mut out = Vec::new();
        serve(&mut &input[..], &mut out, &app, &Ok(())).unwrap();
        assert_eq!(replies(&out)[0]["error"], "Spate did not respond");

        let app = FakeApp::default();
        let input = frames(&[br#"{"type":"ping"}"#, br#"{"type":"download","items":[{"url":"https://e/a"}]}"#]);
        let mut out = Vec::new();
        serve(&mut &input[..], &mut out, &app, &Err("not allowed".into())).unwrap();
        let r = replies(&out);
        assert!(r.iter().all(|m| m["ok"] == false && m["error"] == "not allowed"));
        assert!(app.added.borrow().is_empty());
    }

    #[test]
    fn oversized_or_truncated_input_ends_the_session() {
        let app = FakeApp::default();
        let mut input = (64u32 * 1024 * 1024).to_le_bytes().to_vec();
        input.extend_from_slice(b"{}");
        let mut out = Vec::new();
        assert!(serve(&mut &input[..], &mut out, &app, &Ok(())).is_err());
        assert_eq!(replies(&out)[0]["ok"], false);

        let mut truncated = 100u32.to_le_bytes().to_vec();
        truncated.extend_from_slice(b"{\"type\"");
        assert!(serve(&mut &truncated[..], &mut Vec::new(), &app, &Ok(())).is_err());
    }
}
