//! End-to-end tests of the `spate-cli` binary against a local range-capable
//! HTTP server and an in-process IPC server standing in for the desktop app.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use spate_core::ipc::{self, Request};

const BIN: &str = env!("CARGO_BIN_EXE_spate-cli");

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("spate-cli-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Deterministic pseudo-random content.
fn content(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Minimal HTTP/1.1 server for `/file.bin` honouring `Range: bytes=a-[b]`.
fn http_server(data: Arc<Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let data = data.clone();
            std::thread::spawn(move || {
                let _ = handle_http(stream, &data);
            });
        }
    });
    format!("http://{addr}")
}

fn handle_http(stream: TcpStream, data: &[u8]) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("range")
        {
            range = v.trim().strip_prefix("bytes=").map(str::to_string);
        }
    }
    let mut out = stream;
    let path = request_line.split_whitespace().nth(1).unwrap_or("");
    if path != "/file.bin" {
        return out.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }
    let len = data.len() as u64;
    let (status, start, end) = match range.as_deref().and_then(|r| r.split_once('-')) {
        Some((a, b)) => {
            let a: u64 = a.parse().unwrap_or(0);
            let b: u64 = if b.is_empty() { len - 1 } else { b.parse::<u64>().unwrap_or(len - 1).min(len - 1) };
            ("206 Partial Content", a, b)
        }
        None => ("200 OK", 0, len - 1),
    };
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\n\
         Content-Type: application/octet-stream\r\nConnection: close\r\n",
        end - start + 1
    );
    if status.starts_with("206") {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{len}\r\n"));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    for chunk in data[start as usize..=end as usize].chunks(64 * 1024) {
        out.write_all(chunk)?;
    }
    out.flush()
}

fn cli(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("SPATE_DATA_DIR", data_dir)
        .env("SPATE_APP_EXE", data_dir.join("no-such-app.exe"))
        .env("NO_PROXY", "127.0.0.1,localhost")
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn get_downloads_pauses_and_resumes() {
    let data = Arc::new(content(6 * 1024 * 1024 + 123));
    let base = http_server(data.clone());
    let url = format!("{base}/file.bin");
    let work = temp_dir("get");
    let data_dir = temp_dir("get-data");
    let out_dir = work.to_string_lossy().to_string();

    // Plain multi-connection download.
    let o = cli(&data_dir, &["get", &url, "-d", &out_dir, "-c", "4", "--min-split", "256K", "-o", "full.bin"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(std::fs::read(work.join("full.bin")).unwrap() == *data, "downloaded content differs");

    // Interrupted download: exit 130, partial file + sidecar left behind.
    let o = cli(&data_dir, &["get", &url, "-d", &out_dir, "-c", "4", "--min-split", "256K", "--limit", "1M", "--stop-after-secs", "1.5"]);
    assert_eq!(o.status.code(), Some(130), "{}", text(&o));
    let sidecar = work.join("file.bin.part.json");
    assert!(sidecar.is_file(), "sidecar missing: {}", text(&o));
    assert!(work.join("file.bin.part").is_file());
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
    assert_eq!(saved["url"], url.as_str());
    let done: u64 = saved["resume"]["segments"].as_array().unwrap().iter().map(|s| s["done"].as_u64().unwrap()).sum();
    assert!(done > 0 && done < data.len() as u64, "partial progress expected, got {done}");

    // Same command again resumes and completes.
    let o = cli(&data_dir, &["get", &url, "-d", &out_dir, "-c", "4", "--min-split", "256K"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("Resuming"), "{}", text(&o));
    assert!(std::fs::read(work.join("file.bin")).unwrap() == *data, "resumed content differs");
    assert!(!sidecar.exists() && !work.join("file.bin.part").exists());

    // Checksum verification.
    let o = cli(&data_dir, &["get", &url, "-d", &out_dir, "-o", "bad.bin", "--sha256", &"0".repeat(64)]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).contains("checksum"), "{}", text(&o));

    std::fs::remove_dir_all(&work).ok();
    std::fs::remove_dir_all(&data_dir).ok();
}

#[test]
fn info_and_errors() {
    let data = Arc::new(content(1000));
    let base = http_server(data);
    let data_dir = temp_dir("info-data");
    let o = cli(&data_dir, &["info", &format!("{base}/file.bin")]);
    assert!(o.status.success(), "{}", text(&o));
    let t = text(&o);
    assert!(t.contains("File name:     file.bin") && t.contains("1000 bytes") && t.contains("Resumable:     yes"), "{t}");

    let o = cli(&data_dir, &["info", "--json", &format!("{base}/file.bin")]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["size"], 1000);

    let o = cli(&data_dir, &["info", &format!("{base}/missing")]);
    assert_eq!(o.status.code(), Some(1));
    let o = cli(&data_dir, &["get", "-d", &data_dir.to_string_lossy(), &format!("{base}/missing")]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).contains("404"), "{}", text(&o));
    std::fs::remove_dir_all(&data_dir).ok();
}

fn frame(json: &str) -> Vec<u8> {
    let mut v = Vec::new();
    ipc::write_native_message(&mut v, json.as_bytes()).unwrap();
    v
}

fn host_session(data_dir: &Path, args: &[&str], input: &[u8]) -> Vec<serde_json::Value> {
    let mut child = Command::new(BIN)
        .args(args)
        .env("SPATE_DATA_DIR", data_dir)
        .env("SPATE_APP_EXE", data_dir.join("no-such-app.exe"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    child.wait().unwrap();
    let mut r = &out[..];
    let mut replies = Vec::new();
    while let Some(m) = ipc::read_native_message(&mut r).unwrap() {
        replies.push(serde_json::from_slice(&m).expect("stdout must only carry framed JSON"));
    }
    replies
}

#[test]
fn native_host_and_add_reach_the_app() {
    let data_dir = temp_dir("host-data");
    let origin = format!("chrome-extension://{}/", ipc::CHROMIUM_EXTENSION_ID);

    // App not running and not installed: ping says so, downloads fail cleanly.
    let mut input = frame(r#"{"type":"ping"}"#);
    input.extend(frame(r#"{"type":"download","items":[{"url":"https://example.com/a.zip"}]}"#));
    let r = host_session(&data_dir, &[&origin, "--parent-window=0"], &input);
    assert_eq!(r.len(), 2);
    assert_eq!((r[0]["ok"].as_bool(), r[0]["appRunning"].as_bool()), (Some(true), Some(false)));
    assert_eq!(r[1]["ok"], false);
    assert!(r[1]["error"].as_str().unwrap().contains("not found"), "{:?}", r[1]);

    // Unknown extensions are refused.
    let r = host_session(&data_dir, &["chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/"], &frame(r#"{"type":"ping"}"#));
    assert_eq!(r[0]["ok"], false);
    let r = host_session(&data_dir, &["C:\\m.json", "evil@example.com"], &frame(r#"{"type":"ping"}"#));
    assert_eq!(r[0]["ok"], false);

    // With an app answering IPC, everything is forwarded.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let seen: Arc<Mutex<Vec<Request>>> = Arc::default();
    let s = seen.clone();
    let handler: ipc::Handler = Arc::new(move |req| {
        s.lock().unwrap().push(req);
        ipc::Response::ok()
    });
    let server = rt.block_on(ipc::serve(&data_dir, handler)).unwrap();

    let input = frame(r#"{"type":"download","interactive":true,"items":[{"url":"https://example.com/b.zip","fileName":"b.zip","cookies":"k=v"}]}"#);
    let r = host_session(&data_dir, &["C:\\m.json", ipc::FIREFOX_EXTENSION_ID], &input);
    assert_eq!(r[0]["ok"], true, "{:?}", r[0]);

    let o = cli(&data_dir, &["/n", "/d", "https://example.com/c.iso", "/f", "c.iso"]);
    assert!(o.status.success(), "{}", text(&o));
    let o = cli(&data_dir, &["add", "https://example.com/d.bin", "--referer", "https://example.com/"]);
    assert!(o.status.success(), "{}", text(&o));

    let seen = seen.lock().unwrap().clone();
    let adds: Vec<(Vec<ipc::AddItem>, bool)> = seen
        .into_iter()
        .filter_map(|r| match r {
            Request::Add { items, interactive, .. } => Some((items, interactive)),
            _ => None,
        })
        .collect();
    assert_eq!(adds.len(), 3, "{adds:?}");
    assert_eq!(adds[0].0[0].cookies.as_deref(), Some("k=v"));
    assert!(adds[0].1, "extension asked for the dialog");
    assert_eq!(adds[1].0[0].file_name.as_deref(), Some("c.iso"));
    assert!(!adds[1].1, "/n means silent");
    assert_eq!(adds[2].0[0].referrer.as_deref(), Some("https://example.com/"));
    assert!(adds[2].1, "add without --silent is interactive");

    drop(server);
    std::fs::remove_dir_all(&data_dir).ok();
}

/// A program that took over the port of a stale `ipc.json` and does not
/// know the token. Returns what each connection to it carried.
fn impostor(data_dir: &Path) -> Arc<Mutex<Vec<String>>> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let info = ipc::IpcInfo { port: listener.local_addr().unwrap().port(), token: "c3".repeat(24), pid: 1 };
    std::fs::write(ipc::info_path(data_dir), serde_json::to_vec(&info).unwrap()).unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let s = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let s = s.clone();
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut text = String::new();
                let _ = reader.read_line(&mut text);
                // Agree to everything, the way an old app would answer.
                let _ = (&stream).write_all(b"{\"ok\":true,\"error\":null,\"version\":\"0.1.0\",\"proof\":\"00\"}\n");
                let _ = reader.read_to_string(&mut text);
                s.lock().unwrap().push(text);
            });
        }
    });
    seen
}

#[test]
fn helpers_tell_an_impostor_nothing() {
    let data_dir = temp_dir("impostor-data");
    let seen = impostor(&data_dir);
    let origin = format!("chrome-extension://{}/", ipc::CHROMIUM_EXTENSION_ID);

    let o = cli(&data_dir, &["add", "https://example.com/a.zip", "--cookie", "session=SECRET", "--silent"]);
    assert!(!o.status.success(), "{}", text(&o));
    let input = frame(r#"{"type":"download","items":[{"url":"https://example.com/b.zip","cookies":"session=SECRET"}]}"#);
    let r = host_session(&data_dir, &[&origin, "--parent-window=0"], &input);
    assert_eq!(r[0]["ok"], false, "{:?}", r[0]);
    let o = cli(&data_dir, &["status"]);
    assert!(text(&o).contains("not running"), "{}", text(&o));
    let o = cli(&data_dir, &["show"]);
    assert!(!o.status.success(), "{}", text(&o));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while seen.lock().unwrap().len() < 4 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4, "one connection per helper run: {seen:?}");
    for conn in &seen {
        let v: serde_json::Value = serde_json::from_str(conn.trim()).expect("a single challenge line, nothing after it");
        assert_eq!(v["challenge"].as_str().map(str::len), Some(64), "{conn}");
        assert!(!conn.contains("SECRET") && !conn.contains("c3c3") && !conn.contains("example.com"), "{conn}");
    }
    std::fs::remove_dir_all(&data_dir).ok();
}
