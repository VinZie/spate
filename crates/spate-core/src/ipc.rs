//! Local IPC between the running desktop app and its helpers.
//!
//! The app listens on a random loopback port and writes
//! `{port, token, pid}` to `ipc.json` in the per-user data directory. Helpers
//! (the CLI and the browser's native-messaging host) read that file and send
//! one JSON line per request, carrying the token. Web pages cannot read the
//! token, so unlike an unauthenticated localhost socket they cannot drive the
//! app.
//!
//! The port in a stale `ipc.json` (left by a crash) may since have been taken
//! by another program, possibly another user's. So before the token and the
//! request (which can carry cookies) go out, the client sends a fresh random
//! challenge and the server must answer with
//! `HMAC-SHA256(token, challenge || PROOF_CONTEXT)`, which only a process that
//! read our `ipc.json` can compute:
//!
//! ```text
//! client -> {"challenge": "<64 hex digits>"}
//! server <- {"proof": "<64 hex digits>", "version": "..."}
//! client -> {"token": "...", "op": "add", ...}
//! server <- {"ok": true, ...}
//! ```
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

/// First line of a connection: the client asks the server to prove it
/// knows the token.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Challenge {
    challenge: String,
}

#[derive(Serialize, Deserialize)]
struct Proof {
    proof: String,
    #[serde(default)]
    version: String,
}

/// Domain separation for the server's proof, so it cannot be mistaken for
/// any other use of the token.
const PROOF_CONTEXT: &[u8] = b"spate-ipc v1 server proof";
const CHALLENGE_BYTES: usize = 32;

/// `HMAC-SHA256(token, challenge || PROOF_CONTEXT)`, hex-encoded.
fn proof_for(token: &str, challenge: &[u8]) -> String {
    hex::encode(hmac_sha256(token.as_bytes(), &[challenge, PROOF_CONTEXT]))
}

/// HMAC-SHA256 (RFC 2104) of the concatenation of `parts`.
fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    for part in parts {
        inner.update(part);
    }
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    let mut mac = [0u8; 32];
    mac.copy_from_slice(&outer.finalize());
    mac
}

pub fn read_info(data_dir: &Path) -> Option<IpcInfo> {
    let bytes = std::fs::read(info_path(data_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Send one request to the running app. Nothing but a random challenge is
/// sent until the other end has proven that it knows the token.
pub fn send(data_dir: &Path, request: &Request) -> io::Result<Response> {
    let info = read_info(data_dir).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Spate is not running"))?;
    let addr = SocketAddr::from(([127, 0, 0, 1], info.port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut challenge = [0u8; CHALLENGE_BYTES];
    getrandom::fill(&mut challenge).map_err(|e| io::Error::other(e.to_string()))?;
    write_line(&mut stream, &Challenge { challenge: hex::encode(challenge) })?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let proven = serde_json::from_str::<Proof>(&line)
        .is_ok_and(|p| constant_time_eq(p.proof.as_bytes(), proof_for(&info.token, &challenge).as_bytes()));
    if !proven {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the program on port {} could not prove it is Spate; {} may be stale", info.port, info_path(data_dir).display()),
        ));
    }

    write_line(&mut stream, &Envelope { token: info.token, request: request.clone() })?;
    line.clear();
    reader.read_line(&mut line)?;
    serde_json::from_str(&line).map_err(io::Error::other)
}

fn write_line(w: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    w.write_all(&line)
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
    use tokio::io::AsyncReadExt;
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = tokio::io::BufReader::new(read.take(1 << 20));
    read_line(&mut reader, &mut line).await?;
    // Current clients first ask us to prove who we are; the request follows.
    // A request without a challenge is still served, as before.
    if let Ok(c) = serde_json::from_str::<Challenge>(&line) {
        let bytes = hex::decode(c.challenge.trim()).ok().filter(|b| b.len() == CHALLENGE_BYTES);
        let Some(challenge) = bytes else {
            return write_json(&mut write, &Response::err("bad challenge")).await;
        };
        write_json(&mut write, &Proof { proof: proof_for(token, &challenge), version: VERSION.into() }).await?;
        read_line(&mut reader, &mut line).await?;
    }
    let response = match serde_json::from_str::<Envelope>(&line) {
        Ok(env) if constant_time_eq(env.token.as_bytes(), token.as_bytes()) => {
            tokio::task::spawn_blocking(move || handler(env.request)).await.unwrap_or_else(|e| Response::err(e.to_string()))
        }
        Ok(_) => Response::err("invalid token"),
        Err(e) => Response::err(format!("bad request: {e}")),
    };
    write_json(&mut write, &response).await
}

async fn read_line(r: &mut (impl tokio::io::AsyncBufRead + Unpin), line: &mut String) -> io::Result<()> {
    use tokio::io::AsyncBufReadExt;
    line.clear();
    tokio::time::timeout(Duration::from_secs(10), r.read_line(line))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    Ok(())
}

async fn write_json(w: &mut (impl tokio::io::AsyncWrite + Unpin), value: &impl Serialize) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut out = serde_json::to_vec(value)?;
    out.push(b'\n');
    w.write_all(&out).await
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

    #[test]
    fn hmac_matches_rfc_4231() {
        let hex = |b: [u8; 32]| hex::encode(b);
        assert_eq!(
            hex(hmac_sha256(&[0x0b; 20], &[b"Hi There"])),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(hmac_sha256(b"Jefe", &[b"what do ya want ", b"for nothing?"])),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(hmac_sha256(&[0xaa; 131], &[b"Test Using Larger Than Block-Size Key - Hash Key First"])),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    /// One raw line-protocol exchange with the server at `info`.
    fn exchange(info: &IpcInfo, lines: &[String]) -> Vec<String> {
        let mut stream = TcpStream::connect(("127.0.0.1", info.port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        lines
            .iter()
            .map(|l| {
                stream.write_all(format!("{l}\n").as_bytes()).unwrap();
                let mut reply = String::new();
                reader.read_line(&mut reply).unwrap();
                reply
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_proves_itself_and_requires_token() {
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
        let i = info.clone();
        let replies = tokio::task::spawn_blocking(move || {
            let ping = |token: &str| serde_json::to_string(&Envelope { token: token.into(), request: Request::Ping }).unwrap();
            let challenge = [7u8; CHALLENGE_BYTES];
            let hello = serde_json::to_string(&Challenge { challenge: hex::encode(challenge) }).unwrap();
            let proof = exchange(&i, std::slice::from_ref(&hello)).remove(0);
            let proof: Proof = serde_json::from_str(&proof).unwrap();
            assert_eq!(proof.proof, proof_for(&i.token, &challenge));
            assert_eq!(proof.version, VERSION);
            vec![
                exchange(&i, &[hello.clone(), ping(&"0".repeat(48))]).remove(1),
                exchange(&i, &[hello, ping(&i.token)]).remove(1),
                // Clients that do not ask for a proof are still checked.
                exchange(&i, &[ping("wrong")]).remove(0),
                exchange(&i, &[ping(&i.token)]).remove(0),
                exchange(&i, &[r#"{"challenge":"abcd"}"#.into()]).remove(0),
            ]
        })
        .await
        .unwrap();
        let r: Vec<Response> = replies.iter().map(|r| serde_json::from_str(r).unwrap()).collect();
        assert_eq!(r[0].error.as_deref(), Some("invalid token"));
        assert!(r[1].ok);
        assert_eq!(r[2].error.as_deref(), Some("invalid token"));
        assert!(r[3].ok);
        assert_eq!(r[4].error.as_deref(), Some("bad challenge"));

        // A client holding a wrong token cannot verify the proof and gives up.
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
        .unwrap();
        assert_eq!(forged.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        drop(server);
        assert!(!info_path(&dir).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A program that took over the port of a stale `ipc.json` and answers
    /// the challenge with `reply`. Returns everything the client sent it.
    fn impostor(dir: &Path, reply: impl Fn(&str) -> String + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let info = IpcInfo { port: listener.local_addr().unwrap().port(), token: "a1".repeat(24), pid: 1 };
        std::fs::write(info_path(dir), serde_json::to_vec(&info).unwrap()).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            (&stream).write_all(reply(&first).as_bytes()).unwrap();
            // Keep listening: whatever else arrives would have leaked.
            let mut rest = Vec::new();
            let _ = reader.read_to_end(&mut rest);
            [first.into_bytes(), rest].concat()
        })
    }

    #[test]
    fn client_sends_nothing_sensitive_to_an_impostor() {
        let dir = std::env::temp_dir().join(format!("spate-ipc-impostor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let add = Request::Add {
            items: vec![AddItem { url: "https://example.com/f".into(), cookies: Some("session=SECRET".into()), ..Default::default() }],
            interactive: false,
            start: None,
        };
        let replies: [fn(&str) -> String; 4] = [
            // An old-style server, or anything that does not know the protocol.
            |_| "{\"ok\":true,\"error\":null,\"version\":\"0.1.0\"}\n".into(),
            |_| "{\"proof\":\"00\"}\n".into(),
            // Echoing the challenge back, or proving with another token.
            |c| {
                let c: Challenge = serde_json::from_str(c).unwrap();
                format!("{{\"proof\":\"{}\"}}\n", c.challenge)
            },
            |c| {
                let c: Challenge = serde_json::from_str(c).unwrap();
                let proof = proof_for(&"b2".repeat(24), &hex::decode(c.challenge).unwrap());
                format!("{{\"proof\":\"{proof}\"}}\n")
            },
        ];
        for reply in replies {
            let fake = impostor(&dir, reply);
            let token = read_info(&dir).unwrap().token;
            let err = send(&dir, &add).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
            let seen = String::from_utf8(fake.join().unwrap()).unwrap();
            let challenge: Challenge = serde_json::from_str(seen.trim()).expect("only the challenge was sent");
            assert_eq!(challenge.challenge.len(), 2 * CHALLENGE_BYTES);
            assert!(!seen.contains(&token) && !seen.contains("SECRET") && !seen.contains("example.com"), "{seen}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
