//! End-to-end tests of the HLS downloader against a tiny in-process HTTP/1.1
//! server (keep-alive, byte ranges, injected failures, cookie checks).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockModeEncrypt, KeyIvInit};
use spate_core::download::{DownloadRequest, EngineOptions, Outcome, Phase, Progress};
use spate_core::hls;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

const COOKIE: &str = "session=abc123";
const REFERER: &str = "https://player.example/watch";

#[derive(Clone, Default)]
struct Route {
    body: Vec<u8>,
    content_type: &'static str,
    honor_ranges: bool,
    /// Answer this many requests with 503 before succeeding.
    fail_first: usize,
    delay: Duration,
    /// Never answer.
    hang: bool,
}

impl Route {
    fn new(body: impl Into<Vec<u8>>, content_type: &'static str) -> Self {
        Self { body: body.into(), content_type, ..Default::default() }
    }
}

#[derive(Default)]
struct State {
    routes: Mutex<HashMap<String, Route>>,
    hits: Mutex<HashMap<String, usize>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    /// Requests that arrived without the cookie/referer.
    rejected: AtomicUsize,
}

struct Server {
    base: String,
    state: Arc<State>,
}

impl Server {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(State::default());
        let s = state.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(serve(sock, s.clone()));
            }
        });
        Self { base: format!("http://{addr}"), state }
    }

    fn route(&self, path: &str, route: Route) {
        self.state.routes.lock().unwrap().insert(path.to_string(), route);
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn hits(&self, path: &str) -> usize {
        self.state.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

async fn serve(mut sock: TcpStream, state: Arc<State>) {
    let mut buf = Vec::new();
    loop {
        let end = loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
            let mut tmp = [0u8; 4096];
            match sock.read(&mut tmp).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        buf.drain(..end + 4);
        let mut lines = head.split("\r\n");
        let target = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("/").to_string();
        let path = target.split('?').next().unwrap_or("/").to_string();
        let headers: HashMap<String, String> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let resp = respond(&state, &path, &headers).await;
        if sock.write_all(&resp).await.is_err() {
            return;
        }
    }
}

struct InFlight<'a>(&'a AtomicUsize);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn respond(state: &State, path: &str, headers: &HashMap<String, String>) -> Vec<u8> {
    let route = state.routes.lock().unwrap().get(path).cloned();
    let hit = {
        let mut hits = state.hits.lock().unwrap();
        let n = hits.entry(path.to_string()).or_default();
        *n += 1;
        *n
    };
    let Some(route) = route else { return response(404, "Not Found", &[], b"") };
    if headers.get("cookie").map(String::as_str) != Some(COOKIE)
        || headers.get("referer").map(String::as_str) != Some(REFERER)
    {
        state.rejected.fetch_add(1, Ordering::SeqCst);
        return response(403, "Forbidden", &[], b"");
    }
    let now = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state.max_in_flight.fetch_max(now, Ordering::SeqCst);
    let _guard = InFlight(&state.in_flight);

    if route.hang {
        tokio::time::sleep(Duration::from_secs(120)).await;
    }
    if !route.delay.is_zero() {
        tokio::time::sleep(route.delay).await;
    }
    if hit <= route.fail_first {
        return response(503, "Service Unavailable", &[], b"try again");
    }
    let ct = ("Content-Type", route.content_type.to_string());
    if route.honor_ranges
        && let Some((first, last)) = headers.get("range").and_then(|r| parse_range(r))
    {
        let len = route.body.len();
        let last = last.min(len - 1);
        let content_range = ("Content-Range", format!("bytes {first}-{last}/{len}"));
        return response(206, "Partial Content", &[ct, content_range], &route.body[first..=last]);
    }
    response(200, "OK", &[ct], &route.body)
}

fn parse_range(v: &str) -> Option<(usize, usize)> {
    let (a, b) = v.strip_prefix("bytes=")?.split_once('-')?;
    Some((a.parse().ok()?, if b.is_empty() { usize::MAX } else { b.parse().ok()? }))
}

fn response(code: u16, reason: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

fn encrypt(key: &[u8; 16], iv: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; plain.len() + 16];
    let n = cbc::Encryptor::<aes::Aes128>::new_from_slices(key, iv)
        .unwrap()
        .encrypt_padded_b2b::<Pkcs7>(plain, &mut buf)
        .unwrap()
        .len();
    buf.truncate(n);
    buf
}

/// Distinct, non-block-aligned content per segment.
fn plain_segment(i: usize) -> Vec<u8> {
    (0..3000 + i * 517).map(|j| ((i * 131 + j * 7 + j / 255) % 256) as u8).collect()
}

fn request(url: String, dir: &Path, file_name: Option<&str>) -> DownloadRequest {
    DownloadRequest {
        url,
        headers: vec![("Cookie".into(), COOKIE.into()), ("Referer".into(), REFERER.into())],
        file_name: file_name.map(str::to_string),
        dir: dir.to_path_buf(),
    }
}

fn options(connections: usize) -> EngineOptions {
    EngineOptions { connections, ..Default::default() }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("spate-hls-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn master_playlist_with_aes128_variant() {
    let server = Server::start().await;
    let key1 = [0x11u8; 16];
    let key2: [u8; 16] = *b"0123456789abcdef";
    let explicit_iv: [u8; 16] = std::array::from_fn(|i| i as u8);
    const SEQ: u64 = 100;

    // Segments 0-1 clear, 2-9 key1 with IV = sequence number, 10-17 key2
    // with an explicit IV, 18-19 clear again (METHOD=NONE).
    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n");
    playlist.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{SEQ}\n"));
    let mut expected = Vec::new();
    for i in 0..20usize {
        match i {
            2 => playlist.push_str("#EXT-X-KEY:METHOD=AES-128,URI=\"../keys/k1.bin\"\n"),
            10 => {
                playlist.push_str("#EXT-X-DISCONTINUITY\n");
                playlist.push_str("#EXT-X-KEY:METHOD=AES-128,URI=\"/keys/k2.bin\",IV=0x000102030405060708090A0B0C0D0E0F\n");
            }
            18 => playlist.push_str("#EXT-X-KEY:METHOD=NONE\n"),
            _ => {}
        }
        playlist.push_str("#EXTINF:4.000,\n");
        // Relative, absolute-path and absolute URIs.
        let uri = match i % 3 {
            0 => format!("seg{i}.ts"),
            1 => format!("/stream/hi/seg{i}.ts"),
            _ => server.url(&format!("/stream/hi/seg{i}.ts")),
        };
        playlist.push_str(&format!("{uri}\n"));

        let plain = plain_segment(i);
        let body = match i {
            2..=9 => encrypt(&key1, &u128::from(SEQ + i as u64).to_be_bytes(), &plain),
            10..=17 => encrypt(&key2, &explicit_iv, &plain),
            _ => plain.clone(),
        };
        let mut route = Route::new(body, "video/mp2t");
        route.delay = Duration::from_millis(15 + (i as u64 * 7) % 40);
        if i == 7 {
            route.fail_first = 2;
        }
        server.route(&format!("/stream/hi/seg{i}.ts"), route);
        expected.extend_from_slice(&plain);
    }
    playlist.push_str("#EXT-X-ENDLIST\n");
    server.route("/stream/hi/index.m3u8", Route::new(playlist, "application/vnd.apple.mpegurl"));
    server.route("/stream/keys/k1.bin", Route::new(key1.to_vec(), "application/octet-stream"));
    server.route("/keys/k2.bin", Route::new(key2.to_vec(), "application/octet-stream"));
    server.route(
        "/stream/lo/index.m3u8",
        Route::new("#EXTM3U\n#EXTINF:4,\nlow.ts\n#EXT-X-ENDLIST\n", "application/vnd.apple.mpegurl"),
    );
    server.route(
        "/stream/master.m3u8",
        Route::new(
            "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\"\n\
             lo/index.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\"\n\
             hi/index.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720\n\
             mid/index.m3u8\n",
            "application/vnd.apple.mpegurl",
        ),
    );

    let dir = temp_dir("master");
    let progress = Arc::new(Progress::default());
    let outcome = hls::run(
        request(server.url("/stream/master.m3u8?token=xyz"), &dir, None),
        options(4),
        progress.clone(),
        CancellationToken::new(),
    )
    .await
    .expect("download succeeds");

    let Outcome::Completed { path, size } = outcome else { panic!("expected completion, got {outcome:?}") };
    assert_eq!(path, dir.join("master.ts"));
    assert_eq!(size, expected.len() as u64);
    let got = std::fs::read(&path).unwrap();
    assert!(got == expected, "output differs from the concatenated plaintext ({} vs {} bytes)", got.len(), expected.len());
    assert_eq!(dir_entries(&dir), ["master.ts"], "no partial file left behind");

    // Best variant only; every request authenticated; retries and key cache.
    assert_eq!(server.hits("/stream/lo/index.m3u8"), 0);
    assert_eq!(server.hits("/stream/hi/index.m3u8"), 1);
    assert_eq!(server.state.rejected.load(Ordering::SeqCst), 0, "cookie/referer sent on every request");
    assert_eq!(server.hits("/stream/hi/seg7.ts"), 3, "two failures then success");
    assert_eq!(server.hits("/stream/keys/k1.bin"), 1, "key fetched once");
    assert_eq!(server.hits("/keys/k2.bin"), 1);
    let max = server.state.max_in_flight.load(Ordering::SeqCst);
    assert!((2..=4).contains(&max), "parallel but bounded by connections: {max}");

    let info = progress.info().unwrap();
    assert_eq!(info.file_name, "master.ts");
    assert_eq!(info.file_path, path);
    assert_eq!(info.total, Some(size));
    assert!(!info.resumable);
    assert_eq!(info.content_type.as_deref(), Some("video/mp2t"));
    assert!(info.final_url.ends_with("/stream/hi/index.m3u8"));
    assert_eq!(progress.received(), size);
    assert_eq!(progress.phase(), Phase::Finishing);
    assert_eq!(progress.active_connections(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn fmp4_live_playlist_with_byte_ranges() {
    let server = Server::start().await;
    let init: Vec<u8> = b"ftypisom-fake-moov-init".repeat(10);
    let media: Vec<u8> = (0..4000u32).map(|i| (i % 253) as u8).collect();
    let whole: Vec<u8> = (0..3000u32).map(|i| (i % 7 + 100) as u8).collect();
    let mut init_route = Route::new(init.clone(), "video/mp4");
    init_route.honor_ranges = true;
    server.route("/f/init.mp4", init_route);
    let mut media_route = Route::new(media.clone(), "video/mp4");
    media_route.honor_ranges = true;
    server.route("/f/media.mp4", media_route);
    // This server ignores Range and always sends the whole file.
    server.route("/f/whole.mp4", Route::new(whole.clone(), "video/mp4"));
    // No #EXT-X-ENDLIST: a live playlist, saved as currently listed.
    server.route(
        "/f/index.m3u8",
        Route::new(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:5000\n\
             #EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"200@10\"\n\
             #EXTINF:2.0,\n#EXT-X-BYTERANGE:1000@0\nmedia.mp4\n\
             #EXTINF:2.0,\n#EXT-X-BYTERANGE:1500\nmedia.mp4\n\
             #EXTINF:2.0,\n#EXT-X-BYTERANGE:700@1200\nwhole.mp4\n",
            "application/x-mpegURL",
        ),
    );

    let dir = temp_dir("fmp4");
    let progress = Arc::new(Progress::default());
    let outcome = hls::run(
        request(server.url("/f/index.m3u8"), &dir, Some("My Video.m3u8")),
        options(3),
        progress.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let Outcome::Completed { path, size } = outcome else { panic!("expected completion") };
    assert_eq!(path, dir.join("My Video.mp4"));
    let mut expected = init[10..210].to_vec();
    expected.extend_from_slice(&media[..2500]);
    expected.extend_from_slice(&whole[1200..1900]);
    assert_eq!(std::fs::read(&path).unwrap(), expected);
    assert_eq!(size, expected.len() as u64);
    assert_eq!(progress.info().unwrap().content_type.as_deref(), Some("video/mp4"));
    assert_eq!(dir_entries(&dir), ["My Video.mp4"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn existing_file_gets_a_unique_name() {
    let server = Server::start().await;
    server.route("/v/a.ts", Route::new(b"segment-a".to_vec(), "video/mp2t"));
    server.route(
        "/v/clip.m3u8",
        Route::new("#EXTM3U\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n", "application/vnd.apple.mpegurl"),
    );
    let dir = temp_dir("unique");
    std::fs::write(dir.join("clip.ts"), b"keep me").unwrap();
    let sha = {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(b"segment-a"))
    };
    let opts = EngineOptions { expected_sha256: Some(sha.to_uppercase()), ..options(2) };
    let outcome = hls::run(
        request(server.url("/v/clip.m3u8"), &dir, None),
        opts,
        Arc::new(Progress::default()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let Outcome::Completed { path, .. } = outcome else { panic!("expected completion") };
    assert_eq!(path, dir.join("clip (2).ts"));
    assert_eq!(std::fs::read(&path).unwrap(), b"segment-a");
    assert_eq!(std::fs::read(dir.join("clip.ts")).unwrap(), b"keep me");

    let opts = EngineOptions { expected_sha256: Some("00".repeat(32)), ..options(2) };
    let err = hls::run(
        request(server.url("/v/clip.m3u8"), &dir, None),
        opts,
        Arc::new(Progress::default()),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, spate_core::Error::Checksum { ref actual, .. } if *actual == sha), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sample_aes_is_rejected() {
    let server = Server::start().await;
    server.route(
        "/drm/index.m3u8",
        Route::new(
            "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://asset\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
             #EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n",
            "application/vnd.apple.mpegurl",
        ),
    );
    let dir = temp_dir("drm");
    let err = hls::run(
        request(server.url("/drm/index.m3u8"), &dir, None),
        options(2),
        Arc::new(Progress::default()),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), "encrypted (DRM/SAMPLE-AES) streams are not supported");
    assert!(dir_entries(&dir).is_empty());
    assert_eq!(server.hits("/drm/a.ts"), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn missing_segment_fails_and_cleans_up() {
    let server = Server::start().await;
    server.route("/m/a.ts", Route::new(vec![1u8; 5000], "video/mp2t"));
    server.route(
        "/m/index.m3u8",
        Route::new("#EXTM3U\n#EXTINF:1,\na.ts\n#EXTINF:1,\ngone.ts\n#EXT-X-ENDLIST\n", "application/vnd.apple.mpegurl"),
    );
    let dir = temp_dir("missing");
    let err = hls::run(
        request(server.url("/m/index.m3u8"), &dir, None),
        options(2),
        Arc::new(Progress::default()),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, spate_core::Error::Status(404)), "{err}");
    assert_eq!(server.hits("/m/gone.ts"), 1, "404 is not retried");
    assert!(dir_entries(&dir).is_empty(), "partial file deleted: {:?}", dir_entries(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_stops_promptly_and_keeps_nothing() {
    let server = Server::start().await;
    let mut playlist = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n");
    for i in 0..10 {
        let mut route = Route::new(plain_segment(i), "video/mp2t");
        route.hang = i == 3;
        server.route(&format!("/c/s{i}.ts"), route);
        playlist.push_str(&format!("#EXTINF:4,\ns{i}.ts\n"));
    }
    playlist.push_str("#EXT-X-ENDLIST\n");
    server.route("/c/index.m3u8", Route::new(playlist, "application/vnd.apple.mpegurl"));

    let dir = temp_dir("cancel");
    let progress = Arc::new(Progress::default());
    let cancel = CancellationToken::new();
    let task = tokio::spawn(hls::run(
        request(server.url("/c/index.m3u8"), &dir, Some("cancelled")),
        options(2),
        progress.clone(),
        cancel.clone(),
    ));

    // Wait until segment 3 is stuck and the part file exists.
    let deadline = Instant::now() + Duration::from_secs(10);
    while server.hits("/c/s3.ts") == 0 || progress.received() == 0 {
        assert!(Instant::now() < deadline, "download never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(dir.join("cancelled.ts.part").exists());
    assert!(progress.active_connections() >= 1);
    assert_eq!(progress.phase(), Phase::Downloading);

    let started = Instant::now();
    cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(5), task).await.expect("stops promptly").unwrap();
    assert!(matches!(outcome, Ok(Outcome::Paused)), "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    assert!(dir_entries(&dir).is_empty(), "nothing kept: {:?}", dir_entries(&dir));
    assert_eq!(progress.active_connections(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cancel_before_start_is_paused() {
    let server = Server::start().await;
    let mut route = Route::new("#EXTM3U\n", "application/vnd.apple.mpegurl");
    route.hang = true;
    server.route("/slow/index.m3u8", route);
    let dir = temp_dir("early");
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.cancel();
    });
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        hls::run(request(server.url("/slow/index.m3u8"), &dir, None), options(2), Arc::new(Progress::default()), cancel),
    )
    .await
    .expect("stops promptly");
    assert!(matches!(outcome, Ok(Outcome::Paused)), "{outcome:?}");
    assert!(dir_entries(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
