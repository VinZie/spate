//! Local IPC between the running desktop app and its helpers.
//!
//! The app listens on a random loopback port and writes
//! `{port, token, pid}` to `ipc.json` in the per-user data directory. Helpers
//! (the CLI and the browser's native-messaging host) read that file and send
//! one JSON line per request, carrying the token. Web pages cannot read the
//! token, so unlike an unauthenticated localhost socket they cannot drive the
//! app.
//!
//! Browser extension → native host messages use Chrome's native-messaging
//! framing (u32 little-endian length + UTF-8 JSON); see [`NativeRequest`].

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Native-messaging host name registered with browsers.
pub const NATIVE_HOST_NAME: &str = "com.spate.host";
/// Chromium extension ID fixed by the `key` in `extension/manifest.json`.
pub const CHROMIUM_EXTENSION_ID: &str = "lliejfalegflnfhnefcinabnpbpbbble";
/// Gecko ID of the Firefox build of the extension.
pub const FIREFOX_EXTENSION_ID: &str = "spate@vinzie.github.io";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

pub fn info_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ipc.json")
}

/// One download handed to the app.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AddItem {
    pub url: String,
    pub file_name: Option<String>,
    pub referrer: Option<String>,
    /// `Cookie` header value.
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    pub headers: Vec<(String, String)>,
    pub dir: Option<PathBuf>,
    /// Size reported by the browser, if known.
    pub size: Option<u64>,
    pub mime: Option<String>,
    /// `"http"` (default) or `"hls"` for `.m3u8` streams.
    pub kind: Option<String>,
}

impl AddItem {
    /// All request headers implied by this item.
    pub fn request_headers(&self) -> Vec<(String, String)> {
        let mut h = self.headers.clone();
        if let Some(c) = self.cookies.as_deref().filter(|c| !c.is_empty()) {
            h.push(("Cookie".into(), c.into()));
        }
        if let Some(r) = self.referrer.as_deref().filter(|r| !r.is_empty()) {
            h.push(("Referer".into(), r.into()));
        }
        if let Some(ua) = self.user_agent.as_deref().filter(|u| !u.is_empty()) {
            h.push(("User-Agent".into(), ua.into()));
        }
        h
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    /// Bring the main window to the front.
    Show,
    /// Add downloads. With `interactive`, the app may ask the user to confirm
    /// (name, folder, start now or later); otherwise they start directly.
    Add {
        items: Vec<AddItem>,
        #[serde(default)]
        interactive: bool,
        /// How to start non-interactive adds; `None` = start now.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<crate::manager::StartMode>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub version: String,
}

impl Response {
    pub fn ok() -> Self {
        Self { ok: true, error: None, version: VERSION.into() }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, error: Some(msg.into()), version: VERSION.into() }
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    token: String,
    #[serde(flatten)]
    request: Request,
}

pub fn read_info(data_dir: &Path) -> Option<IpcInfo> {
    let bytes = std::fs::read(info_path(data_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Send one request to the running app.
pub fn send(data_dir: &Path, request: &Request) -> io::Result<Response> {
    let info = read_info(data_dir).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Spate is not running"))?;
    let addr = SocketAddr::from(([127, 0, 0, 1], info.port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = serde_json::to_vec(&Envelope { token: info.token, request: request.clone() })?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    serde_json::from_str(&reply).map_err(io::Error::other)
}

/// Whether an app instance answers on the advertised endpoint.
pub fn is_running(data_dir: &Path) -> bool {
    send(data_dir, &Request::Ping).is_ok_and(|r| r.ok)
}

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// Running IPC server; dropping it stops listening and removes `ipc.json`.
pub struct IpcServer {
    info_file: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.info_file);
    }
}

/// Start serving on a random loopback port. Must be called within a Tokio
/// runtime. The handler runs on a blocking-capable thread.
pub async fn serve(data_dir: &Path, handler: Handler) -> io::Result<IpcServer> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let mut raw = [0u8; 24];
    getrandom::fill(&mut raw).map_err(|e| io::Error::other(e.to_string()))?;
    let token = hex::encode(raw);
    let info = IpcInfo { port, token: token.clone(), pid: std::process::id() };
    let info_file = info_path(data_dir);
    crate::paths::write_atomic(&info_file, &serde_json::to_vec(&info)?)?;

    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else { continue };
            if !peer.ip().is_loopback() {
                continue;
            }
            let handler = handler.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let _ = handle_conn(stream, &token, handler).await;
            });
        }
    });
    Ok(IpcServer { info_file, task })
}

async fn handle_conn(stream: tokio::net::TcpStream, token: &str, handler: Handler) -> io::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = tokio::io::BufReader::new(read.take(1 << 20));
    tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    let response = match serde_json::from_str::<Envelope>(&line) {
        Ok(env) if constant_time_eq(env.token.as_bytes(), token.as_bytes()) => {
            tokio::task::spawn_blocking(move || handler(env.request)).await.unwrap_or_else(|e| Response::err(e.to_string()))
        }
        Ok(_) => Response::err("invalid token"),
        Err(e) => Response::err(format!("bad request: {e}")),
    };
    let mut out = serde_json::to_vec(&response)?;
    out.push(b'\n');
    write.write_all(&out).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------
// Native messaging (browser extension <-> native host)
// ---------------------------------------------------------------------------

/// Messages sent by the browser extension to the native host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum NativeRequest {
    Ping,
    Download {
        items: Vec<NativeItem>,
        #[serde(default)]
        interactive: bool,
    },
}

/// A download as described by the extension (camelCase JSON).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NativeItem {
    pub url: String,
    pub file_name: Option<String>,
    pub referrer: Option<String>,
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    pub file_size: Option<u64>,
    pub mime: Option<String>,
    pub kind: Option<String>,
}

impl From<NativeItem> for AddItem {
    fn from(n: NativeItem) -> Self {
        AddItem {
            url: n.url,
            file_name: n.file_name,
            referrer: n.referrer,
            cookies: n.cookies,
            user_agent: n.user_agent,
            size: n.file_size,
            mime: n.mime,
            kind: n.kind,
            ..Default::default()
        }
    }
}

/// Reply sent back to the extension.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub version: String,
}

/// Maximum message size Chrome allows from a host is 1 MB; inbound is 4 GB
/// but we refuse anything unreasonable.
const MAX_NATIVE_MESSAGE: u32 = 8 * 1024 * 1024;

/// Read one framed message; `Ok(None)` on clean EOF.
pub fn read_native_message(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_NATIVE_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "native message too large"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

pub fn write_native_message(w: &mut impl Write, json: &[u8]) -> io::Result<()> {
    w.write_all(&(json.len() as u32).to_le_bytes())?;
    w.write_all(json)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_framing_roundtrip() {
        let mut buf = Vec::new();
        write_native_message(&mut buf, br#"{"type":"ping"}"#).unwrap();
        let mut r = &buf[..];
        let msg = read_native_message(&mut r).unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<NativeRequest>(&msg).unwrap(), NativeRequest::Ping);
        assert!(read_native_message(&mut r).unwrap().is_none());
    }

    #[test]
    fn native_download_shape() {
        let json = r#"{"type":"download","interactive":true,"items":[{"url":"https://x/y.zip","fileName":"y.zip","cookies":"a=b","fileSize":10}]}"#;
        let NativeRequest::Download { items, interactive } = serde_json::from_str(json).unwrap() else { panic!() };
        assert!(interactive);
        let add: AddItem = items[0].clone().into();
        assert_eq!(add.size, Some(10));
        assert!(add.request_headers().contains(&("Cookie".into(), "a=b".into())));
    }

    #[tokio::test]
    async fn server_requires_token() {
        let dir = std::env::temp_dir().join(format!("spate-ipc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let handler: Handler = Arc::new(|req| match req {
            Request::Ping => Response::ok(),
            _ => Response::err("unexpected"),
        });
        let server = serve(&dir, handler).await.unwrap();
        let d = dir.clone();
        let ok = tokio::task::spawn_blocking(move || send(&d, &Request::Ping)).await.unwrap().unwrap();
        assert!(ok.ok);

        let info = read_info(&dir).unwrap();
        let d = dir.clone();
        let forged = tokio::task::spawn_blocking(move || {
            let mut forged = info.clone();
            forged.token = "0".repeat(48);
            std::fs::write(info_path(&d), serde_json::to_vec(&forged).unwrap()).unwrap();
            let r = send(&d, &Request::Ping);
            std::fs::write(info_path(&d), serde_json::to_vec(&info).unwrap()).unwrap();
            r
        })
        .await
        .unwrap()
        .unwrap();
        assert!(!forged.ok);
        drop(server);
        assert!(!info_path(&dir).exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
