//! HLS (`.m3u8`) stream downloader.
//!
//! CONTRACT: `run` has the same shape as `download::run` (minus resume data)
//! and reports through the same `Progress`, so the manager can treat both
//! kinds alike.
//!
//! Behaviour:
//! - A master playlist (`#EXT-X-STREAM-INF`) is resolved to its best variant:
//!   highest `BANDWIDTH`, ties broken by `RESOLUTION` (pixel count).
//! - Segments are fetched with at most `opts.connections` requests in flight
//!   (a reorder window of twice that many segments is buffered), each retried
//!   with exponential backoff on transient errors, decrypted when the playlist
//!   says `METHOD=AES-128` (CBC, PKCS#7; key fetched once per URI; IV from the
//!   tag or the media sequence number) and appended strictly in playlist order
//!   to `<name>.ts.part`. Streams with an `#EXT-X-MAP` init section (fMP4/CMAF)
//!   are written as init + fragments to `<name>.mp4`; packed-audio streams
//!   (`.aac`, `.mp3`, ...) keep their extension.
//! - Every request (playlists, keys, segments) carries `req.headers`, so
//!   cookies / referer / user agent reach the CDN.
//! - Live playlists (no `#EXT-X-ENDLIST` and not `PLAYLIST-TYPE:VOD`) are
//!   saved as a snapshot: the segments listed when the playlist was fetched
//!   are downloaded and the download then completes. The playlist is not
//!   polled for new segments.
//! - Not resumable (`FileInfo::resumable == false`, no checkpoint): cancelling
//!   stops every request, deletes the partial file and returns
//!   `Outcome::Paused`; resuming means calling `run` again, which restarts
//!   from the first segment.
//! - `SAMPLE-AES`, `SAMPLE-AES-CTR` and DRM key formats are rejected with
//!   [`DRM_UNSUPPORTED`].
//! - No remuxing: MPEG-TS stays MPEG-TS, and alternate audio renditions
//!   (`#EXT-X-MEDIA:TYPE=AUDIO,URI=...`) are not downloaded or merged.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockModeDecrypt, KeyIvInit};
use parking_lot::Mutex;
use reqwest::StatusCode;
use reqwest::header::{self, HeaderMap};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::{OnceCell, Semaphore};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::download::{DownloadRequest, EngineOptions, FileInfo, Outcome, PART_SUFFIX, Phase, Progress};
use crate::error::{Error, Result};
use crate::http::{self, header_str};
use crate::naming;

/// Error message for streams we refuse to download (DRM or sample encryption).
pub const DRM_UNSUPPORTED: &str = "encrypted (DRM/SAMPLE-AES) streams are not supported";

const MAX_PLAYLIST_BYTES: usize = 16 * 1024 * 1024;
const MAX_KEY_BYTES: usize = 1024;
/// Master playlists pointing at further master playlists.
const MAX_MASTER_DEPTH: usize = 3;
const MAX_CONNECTIONS: usize = 32;
/// Segments buffered ahead of the one being written, per connection, so a
/// slow segment does not leave the other connections idle.
const WINDOW_PER_CONNECTION: usize = 2;
const WRITE_BUFFER: usize = 1 << 20;

// ---------------------------------------------------------------------------
// Playlist model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Playlist {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MasterPlaylist {
    pub variants: Vec<Variant>,
    /// `#EXT-X-MEDIA` alternate renditions (audio, subtitles, ...).
    pub renditions: Vec<Rendition>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub uri: Url,
    /// Peak bits/s (`BANDWIDTH`, falling back to `AVERAGE-BANDWIDTH`); 0 if absent.
    pub bandwidth: u64,
    pub resolution: Option<(u32, u32)>,
    pub codecs: Option<String>,
    /// `AUDIO` group id, referring to [`Rendition::group_id`].
    pub audio_group: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rendition {
    /// `TYPE`: `AUDIO`, `VIDEO`, `SUBTITLES` or `CLOSED-CAPTIONS`.
    pub kind: String,
    pub group_id: String,
    pub name: Option<String>,
    pub language: Option<String>,
    /// `None` when the rendition is muxed into the variant streams.
    pub uri: Option<Url>,
    pub default: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaPlaylist {
    pub media_sequence: u64,
    pub target_duration: Option<f64>,
    /// `#EXT-X-ENDLIST` (or `PLAYLIST-TYPE:VOD`) seen: the list is complete.
    pub ended: bool,
    /// Segments in playback order (`#EXT-X-GAP` segments are omitted).
    pub segments: Vec<MediaSegment>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaSegment {
    pub uri: Url,
    /// Seconds, from `#EXTINF`; 0 if missing.
    pub duration: f64,
    /// Media sequence number (also the default AES-128 IV).
    pub sequence: u64,
    pub range: Option<ByteRange>,
    pub encryption: Option<Encryption>,
    /// fMP4 initialization section that applies to this segment.
    pub init: Option<Arc<InitSection>>,
    /// Preceded by `#EXT-X-DISCONTINUITY`.
    pub discontinuity: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InitSection {
    pub uri: Url,
    pub range: Option<ByteRange>,
    /// Key in effect where the `#EXT-X-MAP` tag appears.
    pub encryption: Option<Encryption>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

impl ByteRange {
    pub fn end(&self) -> u64 {
        self.offset + self.length
    }

    fn header_value(&self) -> String {
        format!("bytes={}-{}", self.offset, self.end() - 1)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Encryption {
    /// Whole-segment AES-128-CBC with PKCS#7 padding.
    Aes128 { key_uri: Url, iv: Option<[u8; 16]> },
    /// `SAMPLE-AES`, `SAMPLE-AES-CTR`, or AES-128 with a non-`identity`
    /// `KEYFORMAT` (i.e. a DRM system).
    Unsupported { method: String },
}

impl MasterPlaylist {
    /// Highest `BANDWIDTH`; ties broken by the larger resolution.
    pub fn best_variant(&self) -> Option<&Variant> {
        self.variants
            .iter()
            .max_by_key(|v| (v.bandwidth, v.resolution.map_or(0, |(w, h)| u64::from(w) * u64::from(h))))
    }

    /// The separately delivered audio rendition `variant` would be played
    /// with, if any (its `AUDIO` group has a rendition with its own URI).
    pub fn separate_audio(&self, variant: &Variant) -> Option<&Rendition> {
        let group = variant.audio_group.as_deref()?;
        let mut candidates = self
            .renditions
            .iter()
            .filter(|r| r.kind.eq_ignore_ascii_case("AUDIO") && r.group_id == group && r.uri.is_some());
        let first = candidates.next()?;
        Some(if first.default { first } else { candidates.find(|r| r.default).unwrap_or(first) })
    }
}

impl MediaPlaylist {
    pub fn is_live(&self) -> bool {
        !self.ended
    }

    /// Sum of the segment durations, in seconds.
    pub fn duration(&self) -> f64 {
        self.segments.iter().map(|s| s.duration).sum()
    }
}

/// `.m3u8` URL (query and fragment ignored).
pub fn is_hls_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.to_ascii_lowercase().ends_with(".m3u8")
}

/// HLS playlist MIME types.
pub fn is_hls_content_type(content_type: &str) -> bool {
    let mime = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "application/vnd.apple.mpegurl" | "application/x-mpegurl" | "audio/mpegurl" | "audio/x-mpegurl"
    )
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a master or media playlist; relative URIs are resolved against
/// `base` (the playlist's final URL).
pub fn parse_playlist(text: &str, base: &Url) -> Result<Playlist> {
    let mut lines = text.trim_start_matches('\u{feff}').lines().map(str::trim).filter(|l| !l.is_empty());
    if !lines.next().is_some_and(|l| l.starts_with("#EXTM3U")) {
        return Err(Error::Other("not an HLS playlist (missing #EXTM3U header)".into()));
    }
    let lines: Vec<&str> = lines.collect();
    if lines.iter().any(|l| tag_value(l, "#EXT-X-STREAM-INF").is_some()) {
        parse_master(&lines, base).map(Playlist::Master)
    } else {
        parse_media(&lines, base).map(Playlist::Media)
    }
}

fn parse_master(lines: &[&str], base: &Url) -> Result<MasterPlaylist> {
    let mut out = MasterPlaylist::default();
    let mut pending: Option<Attributes> = None;
    for &line in lines {
        if let Some(v) = tag_value(line, "#EXT-X-STREAM-INF") {
            pending = Some(Attributes::parse(v));
        } else if let Some(v) = tag_value(line, "#EXT-X-MEDIA") {
            let a = Attributes::parse(v);
            out.renditions.push(Rendition {
                kind: a.get("TYPE").unwrap_or("").to_ascii_uppercase(),
                group_id: a.get("GROUP-ID").unwrap_or("").to_string(),
                name: a.get("NAME").map(str::to_string),
                language: a.get("LANGUAGE").map(str::to_string),
                uri: a.get("URI").map(|u| resolve(base, u)).transpose()?,
                default: a.get("DEFAULT").is_some_and(|d| d.eq_ignore_ascii_case("YES")),
            });
        } else if line.starts_with('#') {
            // #EXT-X-I-FRAME-STREAM-INF, #EXT-X-SESSION-*, comments...
        } else if let Some(a) = pending.take() {
            let number = |k: &str| a.get(k).and_then(|v| v.trim().parse::<u64>().ok());
            out.variants.push(Variant {
                uri: resolve(base, line)?,
                bandwidth: number("BANDWIDTH").or_else(|| number("AVERAGE-BANDWIDTH")).unwrap_or(0),
                resolution: a.get("RESOLUTION").and_then(parse_resolution),
                codecs: a.get("CODECS").map(str::to_string),
                audio_group: a.get("AUDIO").map(str::to_string),
            });
        }
    }
    Ok(out)
}

fn parse_media(lines: &[&str], base: &Url) -> Result<MediaPlaylist> {
    let mut out = MediaPlaylist::default();
    let mut sequence = 0u64;
    let mut seen_uri = false;
    let mut duration: Option<f64> = None;
    let mut range: Option<(u64, Option<u64>)> = None;
    // End of the previous segment's sub-range, for `#EXT-X-BYTERANGE:<n>`
    // without an offset.
    let mut prev_end: Option<(Url, u64)> = None;
    let mut encryption: Option<Encryption> = None;
    let mut init: Option<Arc<InitSection>> = None;
    let mut discontinuity = false;
    let mut gap = false;
    let mut vod = false;

    for &line in lines {
        if let Some(v) = tag_value(line, "#EXTINF") {
            duration = v
                .split(',')
                .next()
                .and_then(|d| d.trim().parse::<f64>().ok())
                .filter(|d| d.is_finite() && *d >= 0.0);
        } else if let Some(v) = tag_value(line, "#EXT-X-MEDIA-SEQUENCE") {
            out.media_sequence = v.trim().parse().map_err(|_| invalid("#EXT-X-MEDIA-SEQUENCE"))?;
            if !seen_uri {
                sequence = out.media_sequence;
            }
        } else if let Some(v) = tag_value(line, "#EXT-X-TARGETDURATION") {
            out.target_duration = v.trim().parse().ok();
        } else if let Some(v) = tag_value(line, "#EXT-X-BYTERANGE") {
            range = Some(parse_byterange(v)?);
        } else if let Some(v) = tag_value(line, "#EXT-X-KEY") {
            encryption = parse_key(v, base)?;
        } else if let Some(v) = tag_value(line, "#EXT-X-MAP") {
            let a = Attributes::parse(v);
            let uri = resolve(base, a.get("URI").ok_or_else(|| invalid("#EXT-X-MAP (no URI)"))?)?;
            let range = a
                .get("BYTERANGE")
                .map(parse_byterange)
                .transpose()?
                .map(|(length, offset)| ByteRange { offset: offset.unwrap_or(0), length });
            init = Some(Arc::new(InitSection { uri, range, encryption: encryption.clone() }));
        } else if tag_value(line, "#EXT-X-DISCONTINUITY").is_some() {
            discontinuity = true;
        } else if tag_value(line, "#EXT-X-GAP").is_some() {
            gap = true;
        } else if tag_value(line, "#EXT-X-ENDLIST").is_some() {
            out.ended = true;
        } else if let Some(v) = tag_value(line, "#EXT-X-PLAYLIST-TYPE") {
            vod |= v.trim().eq_ignore_ascii_case("VOD");
        } else if line.starts_with('#') {
            // Unknown or irrelevant tag (PROGRAM-DATE-TIME, PART, comments...).
        } else {
            seen_uri = true;
            let uri = resolve(base, line)?;
            let range = range.take().map(|(length, offset)| {
                let implicit = prev_end.as_ref().filter(|(u, _)| *u == uri).map(|(_, end)| *end);
                ByteRange { offset: offset.or(implicit).unwrap_or(0), length }
            });
            prev_end = range.map(|r| (uri.clone(), r.end()));
            if gap {
                // Content is missing on the server; the next segment follows
                // a jump in the timeline.
                gap = false;
                discontinuity = true;
            } else {
                out.segments.push(MediaSegment {
                    uri,
                    duration: duration.unwrap_or(0.0),
                    sequence,
                    range,
                    encryption: encryption.clone(),
                    init: init.clone(),
                    discontinuity,
                });
                discontinuity = false;
            }
            duration = None;
            sequence += 1;
        }
    }
    out.ended |= vod;
    Ok(out)
}

fn parse_key(attrs: &str, base: &Url) -> Result<Option<Encryption>> {
    let a = Attributes::parse(attrs);
    let method = a.get("METHOD").unwrap_or("NONE").trim().to_ascii_uppercase();
    match method.as_str() {
        "NONE" => Ok(None),
        "AES-128" => {
            let format = a.get("KEYFORMAT").unwrap_or("identity").trim();
            if !format.eq_ignore_ascii_case("identity") {
                return Ok(Some(Encryption::Unsupported { method: format!("AES-128 ({format})") }));
            }
            let uri = a.get("URI").ok_or_else(|| invalid("#EXT-X-KEY (AES-128 without URI)"))?;
            let key_uri = resolve(base, uri)?;
            let iv = a.get("IV").map(parse_iv).transpose()?;
            Ok(Some(Encryption::Aes128 { key_uri, iv }))
        }
        _ => Ok(Some(Encryption::Unsupported { method })),
    }
}

/// `<length>[@<offset>]`.
fn parse_byterange(v: &str) -> Result<(u64, Option<u64>)> {
    let v = v.trim();
    let (length, offset) = match v.split_once('@') {
        Some((l, o)) => (l, Some(o)),
        None => (v, None),
    };
    let length: u64 = length.trim().parse().ok().filter(|&l| l > 0).ok_or_else(|| invalid("BYTERANGE"))?;
    let offset = offset.map(|o| o.trim().parse::<u64>().map_err(|_| invalid("BYTERANGE"))).transpose()?;
    if offset.is_some_and(|o| o.checked_add(length).is_none()) {
        return Err(invalid("BYTERANGE"));
    }
    Ok((length, offset))
}

/// `0x` + up to 32 hex digits, right-aligned in 16 bytes.
fn parse_iv(v: &str) -> Result<[u8; 16]> {
    let v = v.trim();
    let digits = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")).ok_or_else(|| invalid("IV"))?;
    if digits.is_empty() || digits.len() > 32 {
        return Err(invalid("IV"));
    }
    let mut iv = [0u8; 16];
    hex::decode_to_slice(format!("{digits:0>32}"), &mut iv).map_err(|_| invalid("IV"))?;
    Ok(iv)
}

fn parse_resolution(v: &str) -> Option<(u32, u32)> {
    let (w, h) = v.trim().split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// The value of tag `name` on `line` (`""` for tags without a value).
/// Matches whole tag names only, so `#EXT-X-MEDIA` does not match
/// `#EXT-X-MEDIA-SEQUENCE`.
fn tag_value<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(name)?;
    if rest.is_empty() { Some(rest) } else { rest.strip_prefix(':') }
}

fn resolve(base: &Url, uri: &str) -> Result<Url> {
    base.join(uri.trim()).map_err(|e| Error::InvalidUrl(format!("{uri}: {e}")))
}

fn invalid(what: &str) -> Error {
    Error::Other(format!("invalid HLS playlist: bad {what}"))
}

/// `KEY=value,KEY="quoted, value",...` attribute list.
#[derive(Debug, Default)]
struct Attributes(Vec<(String, String)>);

impl Attributes {
    fn parse(s: &str) -> Self {
        let mut out = Vec::new();
        let mut rest = s.trim();
        while !rest.is_empty() {
            let Some(eq) = rest.find('=') else { break };
            let key = rest[..eq].trim().to_ascii_uppercase();
            rest = rest[eq + 1..].trim_start();
            let value = if let Some(quoted) = rest.strip_prefix('"') {
                let end = quoted.find('"').unwrap_or(quoted.len());
                let value = quoted[..end].to_string();
                let after = quoted.get(end + 1..).unwrap_or("");
                rest = after.split_once(',').map_or("", |(_, r)| r);
                value
            } else {
                let (value, r) = rest.split_once(',').unwrap_or((rest, ""));
                rest = r;
                value.trim().to_string()
            };
            out.push((key, value));
            rest = rest.trim_start();
        }
        Self(out)
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

// ---------------------------------------------------------------------------
// Output naming and decryption helpers
// ---------------------------------------------------------------------------

/// Container the concatenated segments form.
fn output_ext(media: &MediaPlaylist) -> &'static str {
    if media.segments.iter().any(|s| s.init.is_some()) {
        return "mp4";
    }
    let first = media.segments.first().and_then(|s| naming::from_url(&s.uri)).unwrap_or_default();
    match naming::split_ext(&first).1.to_ascii_lowercase().as_str() {
        "aac" => "aac",
        "mp3" => "mp3",
        "ac3" => "ac3",
        "ec3" => "ec3",
        _ => "ts",
    }
}

fn content_type_for(ext: &str) -> &'static str {
    match ext {
        "mp4" => "video/mp4",
        "aac" => "audio/aac",
        "mp3" => "audio/mpeg",
        "ac3" => "audio/ac3",
        "ec3" => "audio/eac3",
        _ => "video/mp2t",
    }
}

/// `requested` (sanitized) or the playlist URL's name, with its extension
/// set to `ext`. Media/playlist extensions are replaced, anything else is
/// kept as part of the name (`Show.S01` -> `Show.S01.ts`).
fn output_name(requested: Option<&str>, playlist_url: &Url, ext: &str) -> String {
    const REPLACEABLE: &[&str] = &[
        "m3u8", "m3u", "ts", "mts", "m2ts", "mp4", "m4v", "m4a", "m4s", "mov", "mkv", "webm", "flv", "avi", "aac",
        "mp3", "ac3", "ec3",
    ];
    let base = requested
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(naming::sanitize)
        .or_else(|| naming::from_url(playlist_url))
        .unwrap_or_else(|| "video".to_string());
    let (stem, current) = naming::split_ext(&base);
    let current = current.to_ascii_lowercase();
    let name = if current == ext {
        base.clone()
    } else if REPLACEABLE.contains(&current.as_str()) {
        format!("{stem}.{ext}")
    } else {
        format!("{base}.{ext}")
    };
    naming::sanitize(&name)
}

fn default_iv(sequence: u64) -> [u8; 16] {
    u128::from(sequence).to_be_bytes()
}

/// AES-128-CBC with PKCS#7 padding, in place.
pub(crate) fn decrypt_aes128(key: &[u8; 16], iv: &[u8; 16], mut data: Vec<u8>) -> Result<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err(Error::Other(format!(
            "AES-128 encrypted segment of {} bytes is not a whole number of blocks",
            data.len()
        )));
    }
    let decryptor = cbc::Decryptor::<aes::Aes128>::new_from_slices(key, iv)
        .map_err(|_| Error::Other("invalid AES-128 key or IV length".into()))?;
    let len = decryptor
        .decrypt_padded::<Pkcs7>(&mut data)
        .map_err(|_| Error::Other("AES-128 decryption failed (bad padding: wrong key or IV?)".into()))?
        .len();
    data.truncate(len);
    Ok(data)
}

fn check_scheme(url: &Url) -> Result<()> {
    if matches!(url.scheme(), "http" | "https") {
        Ok(())
    } else {
        Err(Error::InvalidUrl(format!("unsupported scheme `{}` in {url}", url.scheme())))
    }
}

fn cancelled() -> Error {
    Error::Other("cancelled".into())
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(300u64 << attempt.min(6)).min(Duration::from_secs(15))
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

/// Download the stream at `req.url` (master or media playlist) into a single
/// `.ts` (or `.mp4` for fMP4) file in `req.dir`, using `opts.connections`
/// parallel segment fetches. See the module docs for live streams,
/// encryption and cancellation semantics.
pub async fn run(
    req: DownloadRequest,
    opts: EngineOptions,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> Result<Outcome> {
    progress.set_phase(Phase::Connecting);
    // Always a fresh start (not resumable), even if `progress` is reused.
    progress.set_received(0);
    let url = Url::parse(req.url.trim()).map_err(|e| Error::InvalidUrl(e.to_string()))?;
    check_scheme(&url)?;
    let ctx = Arc::new(Ctx::new(&req, &opts, progress.clone(), &cancel)?);

    let (playlist_url, media) = match ctx.load_media_playlist(url.clone()).await {
        Ok(v) => v,
        Err(_) if cancel.is_cancelled() => return Ok(Outcome::Paused),
        Err(e) => return Err(e),
    };
    ensure_supported(&media)?;
    if media.is_live() {
        tracing::info!(
            segments = media.segments.len(),
            "live HLS playlist: saving the segments currently listed"
        );
    }

    let ext = output_ext(&media);
    let name = output_name(req.file_name.as_deref(), &url, ext);
    std::fs::create_dir_all(&req.dir)?;
    let file_path = naming::unique_path(&req.dir, &name, PART_SUFFIX);
    let part_path = naming::with_suffix(&file_path, PART_SUFFIX);
    let jobs = build_jobs(&media);
    let exact_total = exact_total(&jobs);
    let info = FileInfo {
        file_name: file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name),
        file_path: file_path.clone(),
        total: exact_total,
        resumable: false,
        content_type: Some(content_type_for(ext).to_string()),
        final_url: playlist_url.to_string(),
    };
    progress.set_info(info.clone());
    progress.set_phase(Phase::Downloading);

    let estimate = Estimate::new(info, &media, exact_total.is_some());
    match download_all(&ctx, jobs, &part_path, estimate).await {
        Ok((size, mut info)) => {
            let path = finalize(&file_path, &part_path)?;
            info.total = Some(size);
            info.file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(info.file_name);
            info.file_path = path.clone();
            progress.set_info(info);
            progress.set_received(size);
            if let Some(expected) = opts.expected_sha256 {
                progress.set_phase(Phase::Verifying);
                let p = path.clone();
                let actual = tokio::task::spawn_blocking(move || crate::download::sha256_file(&p))
                    .await
                    .map_err(|e| Error::Other(e.to_string()))??;
                if !actual.eq_ignore_ascii_case(expected.trim()) {
                    return Err(Error::Checksum { expected, actual });
                }
            }
            Ok(Outcome::Completed { path, size })
        }
        Err(e) => {
            remove_partial(&part_path).await;
            if cancel.is_cancelled() { Ok(Outcome::Paused) } else { Err(e) }
        }
    }
}

fn ensure_supported(media: &MediaPlaylist) -> Result<()> {
    let drm = |e: &Option<Encryption>| matches!(e, Some(Encryption::Unsupported { .. }));
    if media.segments.iter().any(|s| drm(&s.encryption) || s.init.as_ref().is_some_and(|i| drm(&i.encryption))) {
        return Err(Error::Other(DRM_UNSUPPORTED.into()));
    }
    if media.segments.is_empty() {
        return Err(Error::Other("the HLS playlist contains no media segments".into()));
    }
    Ok(())
}

/// One fetch in output order: an init section (written whenever it changes)
/// or a media segment.
#[derive(Clone, Debug)]
enum Job {
    Init { section: Arc<InitSection>, sequence: u64 },
    Segment(Box<MediaSegment>),
}

fn build_jobs(media: &MediaPlaylist) -> Vec<Job> {
    let mut jobs = Vec::with_capacity(media.segments.len() + 1);
    let mut current: Option<&Arc<InitSection>> = None;
    for seg in &media.segments {
        if let Some(init) = &seg.init
            && current.is_none_or(|c| **c != **init)
        {
            jobs.push(Job::Init { section: init.clone(), sequence: seg.sequence });
            current = Some(init);
        }
        jobs.push(Job::Segment(Box::new(seg.clone())));
    }
    jobs
}

/// Known up front only when every fetch is a byte range.
fn exact_total(jobs: &[Job]) -> Option<u64> {
    jobs.iter()
        .map(|j| match j {
            Job::Init { section, .. } => section.range.map(|r| r.length),
            Job::Segment(s) => s.range.map(|r| r.length),
        })
        .sum()
}

struct Fetched {
    data: Vec<u8>,
    /// Bytes transferred (before decryption).
    raw_len: u64,
    /// `None` for init sections.
    duration: Option<f64>,
}

/// Extrapolates the total size from the segments received so far.
struct Estimate {
    info: FileInfo,
    exact: bool,
    segments: usize,
    total_duration: Option<f64>,
    init_bytes: u64,
    seg_bytes: u64,
    seg_done: usize,
    dur_done: f64,
}

impl Estimate {
    fn new(info: FileInfo, media: &MediaPlaylist, exact: bool) -> Self {
        let all_timed = media.segments.iter().all(|s| s.duration > 0.0);
        Self {
            info,
            exact,
            segments: media.segments.len(),
            total_duration: all_timed.then(|| media.duration()),
            init_bytes: 0,
            seg_bytes: 0,
            seg_done: 0,
            dur_done: 0.0,
        }
    }

    fn record(&mut self, f: &Fetched, progress: &Progress) {
        if self.exact {
            return;
        }
        match f.duration {
            None => self.init_bytes += f.raw_len,
            Some(d) => {
                self.seg_bytes += f.raw_len;
                self.seg_done += 1;
                self.dur_done += d;
            }
        }
        if self.seg_done == 0 {
            return;
        }
        let media = match self.total_duration {
            Some(total) if self.dur_done > 0.0 => self.seg_bytes as f64 / self.dur_done * total,
            _ => self.seg_bytes as f64 / self.seg_done as f64 * self.segments as f64,
        };
        let estimate = (self.init_bytes as f64 + media) as u64;
        self.info.total = Some(estimate.max(progress.received()));
        progress.set_info(self.info.clone());
    }
}

/// Fetch every job with bounded parallelism and append the results to
/// `part_path` in order. Returns the bytes written.
async fn download_all(
    ctx: &Arc<Ctx>,
    jobs: Vec<Job>,
    part_path: &Path,
    mut estimate: Estimate,
) -> Result<(u64, FileInfo)> {
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(part_path)?;
    let mut out = BufWriter::with_capacity(WRITE_BUFFER, tokio::fs::File::from_std(file));
    let permits = Arc::new(Semaphore::new(ctx.connections));
    let window = ctx.connections * WINDOW_PER_CONNECTION;
    let mut pending = jobs.into_iter();
    // Dropping this aborts every outstanding fetch.
    let mut in_flight: VecDeque<AbortOnDrop<Option<Fetched>>> = VecDeque::with_capacity(window);
    let mut written = 0u64;

    let pumped: Result<()> = async {
        loop {
            while in_flight.len() < window {
                let Some(job) = pending.next() else { break };
                in_flight.push_back(spawn_job(ctx.clone(), job, permits.clone()));
            }
            let Some(next) = in_flight.front_mut() else { return Ok(()) };
            let joined = tokio::select! {
                biased;
                _ = ctx.stop.cancelled() => return Err(ctx.stop_reason()),
                r = next => r,
            };
            in_flight.pop_front();
            let fetched = match joined {
                Ok(Some(f)) => f,
                Ok(None) => return Err(ctx.stop_reason()),
                Err(e) => return Err(Error::Other(format!("segment task failed: {e}"))),
            };
            out.write_all(&fetched.data).await?;
            written += fetched.data.len() as u64;
            estimate.record(&fetched, &ctx.progress);
        }
    }
    .await;
    if let Err(e) = pumped {
        // Stop and join every fetch so none outlives `run` (no stray
        // requests, active-connection count back to zero).
        ctx.stop.cancel();
        for task in in_flight.drain(..) {
            task.0.abort();
            let _ = task.await;
        }
        return Err(e);
    }

    ctx.progress.set_phase(Phase::Finishing);
    out.flush().await?;
    let file = out.into_inner();
    file.sync_all().await?;
    drop(file);
    Ok((written, estimate.info))
}

fn spawn_job(ctx: Arc<Ctx>, job: Job, permits: Arc<Semaphore>) -> AbortOnDrop<Option<Fetched>> {
    AbortOnDrop(tokio::spawn(async move {
        let _permit = tokio::select! {
            p = permits.acquire_owned() => p.ok()?,
            _ = ctx.stop.cancelled() => return None,
        };
        match ctx.fetch_job(&job).await {
            Ok(f) => Some(f),
            Err(e) => {
                if !ctx.stop.is_cancelled() {
                    ctx.fail(e);
                }
                None
            }
        }
    }))
}

fn finalize(file_path: &Path, part_path: &Path) -> Result<PathBuf> {
    // Someone may have created the destination while we were downloading.
    let target = if file_path.exists() {
        let dir = file_path.parent().map(PathBuf::from).unwrap_or_default();
        let name = file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        naming::unique_path(&dir, &name, PART_SUFFIX)
    } else {
        file_path.to_path_buf()
    };
    std::fs::rename(part_path, &target)?;
    Ok(target)
}

/// Delete the partial file, retrying briefly while a write-behind of the
/// just-dropped handle still holds it open (Windows refuses the delete then).
async fn remove_partial(path: &Path) {
    for _ in 0..20 {
        match std::fs::remove_file(path) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    tracing::warn!(path = %path.display(), "could not delete partial HLS file");
}

struct Ctx {
    headers: HeaderMap,
    opts: EngineOptions,
    connections: usize,
    progress: Arc<Progress>,
    /// Cancelled by the user (parent token) or by a fatal error.
    stop: CancellationToken,
    fatal: Mutex<Option<Error>>,
    clients: ClientPool,
    keys: Mutex<HashMap<Url, Arc<OnceCell<[u8; 16]>>>>,
}

impl Ctx {
    fn new(req: &DownloadRequest, opts: &EngineOptions, progress: Arc<Progress>, cancel: &CancellationToken) -> Result<Self> {
        let connections = opts.connections.clamp(1, MAX_CONNECTIONS);
        let idle = (0..connections).map(|_| http::build_client(&opts.net)).collect::<Result<Vec<_>>>()?;
        Ok(Self {
            headers: http::header_map(&req.headers),
            opts: opts.clone(),
            connections,
            progress,
            stop: cancel.child_token(),
            fatal: Mutex::new(None),
            clients: ClientPool { shared: http::build_client(&opts.net)?, idle: Mutex::new(idle), max_idle: connections },
            keys: Mutex::new(HashMap::new()),
        })
    }

    fn fail(&self, e: Error) {
        let mut fatal = self.fatal.lock();
        if fatal.is_none() {
            *fatal = Some(e);
        }
        drop(fatal);
        self.stop.cancel();
    }

    fn stop_reason(&self) -> Error {
        self.fatal.lock().take().unwrap_or_else(cancelled)
    }

    /// Fetch `url`, following master playlists to their best variant.
    async fn load_media_playlist(&self, mut url: Url) -> Result<(Url, MediaPlaylist)> {
        for _ in 0..=MAX_MASTER_DEPTH {
            let (body, final_url) = self.fetch(&url, None, MAX_PLAYLIST_BYTES, false).await?;
            let text = String::from_utf8_lossy(&body);
            match parse_playlist(&text, &final_url)? {
                Playlist::Media(media) => return Ok((final_url, media)),
                Playlist::Master(master) => {
                    let variant = master
                        .best_variant()
                        .ok_or_else(|| Error::Other("the HLS master playlist lists no variant streams".into()))?;
                    if let Some(audio) = master.separate_audio(variant) {
                        tracing::warn!(
                            audio = %audio.uri.as_ref().map(Url::as_str).unwrap_or_default(),
                            "HLS variant has a separate audio rendition, which is not downloaded"
                        );
                    }
                    tracing::debug!(variant = %variant.uri, bandwidth = variant.bandwidth, "selected HLS variant");
                    url = variant.uri.clone();
                }
            }
        }
        Err(Error::Other("too many nested HLS master playlists".into()))
    }

    async fn fetch_job(&self, job: &Job) -> Result<Fetched> {
        let (uri, range, encryption, sequence, duration) = match job {
            Job::Init { section, sequence } => (&section.uri, section.range, &section.encryption, *sequence, None),
            Job::Segment(s) => (&s.uri, s.range, &s.encryption, s.sequence, Some(s.duration)),
        };
        let key = match encryption {
            None => None,
            Some(Encryption::Aes128 { key_uri, iv }) => {
                Some((self.key(key_uri).await?, iv.unwrap_or_else(|| default_iv(sequence))))
            }
            Some(Encryption::Unsupported { .. }) => return Err(Error::Other(DRM_UNSUPPORTED.into())),
        };
        let (raw, _) = self.fetch(uri, range, usize::MAX, true).await?;
        let raw_len = raw.len() as u64;
        let data = match key {
            Some((key, iv)) => decrypt_aes128(&key, &iv, raw)?,
            None => raw,
        };
        Ok(Fetched { data, raw_len, duration })
    }

    /// AES-128 key for `uri`, fetched once and shared by all segments.
    async fn key(&self, uri: &Url) -> Result<[u8; 16]> {
        let cell = self.keys.lock().entry(uri.clone()).or_default().clone();
        let key = cell
            .get_or_try_init(|| async {
                let (raw, _) = self.fetch(uri, None, MAX_KEY_BYTES, false).await?;
                <[u8; 16]>::try_from(raw.as_slice()).map_err(|_| {
                    Error::Other(format!("invalid AES-128 key from {uri}: expected 16 bytes, got {}", raw.len()))
                })
            })
            .await?;
        Ok(*key)
    }

    /// GET with retries and backoff on transient errors. `track` reports the
    /// body bytes to `Progress` (a retried request is only counted beyond
    /// what an earlier attempt already reported).
    async fn fetch(&self, url: &Url, range: Option<ByteRange>, cap: usize, track: bool) -> Result<(Vec<u8>, Url)> {
        check_scheme(url)?;
        let mut counted = 0u64;
        let mut attempt = 0u32;
        loop {
            let err = match self.fetch_once(url, range, cap, track, &mut counted).await {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            if self.stop.is_cancelled() || !err.is_transient() || attempt >= self.opts.max_retries {
                return Err(err);
            }
            let wait = backoff(attempt);
            attempt += 1;
            tracing::debug!(%url, attempt, error = %err, ?wait, "HLS request failed, retrying");
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.stop.cancelled() => return Err(cancelled()),
            }
        }
    }

    async fn fetch_once(
        &self,
        url: &Url,
        range: Option<ByteRange>,
        cap: usize,
        track: bool,
        counted: &mut u64,
    ) -> Result<(Vec<u8>, Url)> {
        let client = self.clients.lease();
        let _active = track.then(|| ActiveGuard::new(self.progress.active_counter()));
        let mut rb = client.get(url.clone()).headers(self.headers.clone());
        if let Some(r) = range {
            rb = rb.header(header::RANGE, r.header_value());
        }
        let mut resp = tokio::select! {
            biased;
            _ = self.stop.cancelled() => return Err(cancelled()),
            r = rb.send() => r?,
        };
        let status = resp.status();
        // `skip` leading bytes are dropped, at most `want` bytes are kept.
        let (mut skip, want) = match range {
            Some(r) if status == StatusCode::PARTIAL_CONTENT => {
                let first = header_str(resp.headers(), header::CONTENT_RANGE)
                    .and_then(http::parse_content_range)
                    .map(|(first, _, _)| first);
                if first != Some(r.offset) {
                    return Err(Error::Other(format!("{url}: server returned the wrong byte range")));
                }
                (0, Some(r.length))
            }
            // Range ignored: cut our slice out of the full body.
            Some(r) if status == StatusCode::OK => (r.offset, Some(r.length)),
            _ if status.is_success() => (0, None),
            _ => return Err(Error::Status(status.as_u16())),
        };

        let hint = want.or(resp.content_length()).unwrap_or(0).min(64 << 20).min(cap as u64);
        let mut data = Vec::with_capacity(hint as usize);
        let read_timeout = Duration::from_secs(self.opts.net.read_timeout_secs.max(1));
        let mut seen = 0u64;
        loop {
            let next = tokio::select! {
                biased;
                _ = self.stop.cancelled() => return Err(cancelled()),
                r = tokio::time::timeout(read_timeout, resp.chunk()) => r,
            };
            let chunk = match next {
                Err(_) => return Err(Error::Stalled(read_timeout.as_secs())),
                Ok(Err(e)) => return Err(e.into()),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(c))) => c,
            };
            seen += chunk.len() as u64;
            if track && seen > *counted {
                self.progress.add_received(seen - *counted);
                *counted = seen;
            }
            for limiter in &self.opts.limiters {
                tokio::select! {
                    _ = limiter.acquire(chunk.len() as u64) => {}
                    _ = self.stop.cancelled() => return Err(cancelled()),
                }
            }
            let mut part = &chunk[..];
            if skip > 0 {
                let n = skip.min(part.len() as u64) as usize;
                part = &part[n..];
                skip -= n as u64;
            }
            if let Some(w) = want {
                let room = (w - data.len() as u64).min(part.len() as u64) as usize;
                part = &part[..room];
            }
            if data.len() + part.len() > cap {
                return Err(Error::Other(format!("{url}: response is too large")));
            }
            data.extend_from_slice(part);
            if want.is_some_and(|w| data.len() as u64 >= w) {
                break;
            }
        }
        if want.is_some_and(|w| (data.len() as u64) < w) {
            return Err(Error::Truncated);
        }
        Ok((data, resp.url().clone()))
    }
}

/// One client per connection, so parallel fetches use separate keep-alive
/// connections (as the HTTP engine does) instead of reconnecting each time.
struct ClientPool {
    idle: Mutex<Vec<reqwest::Client>>,
    /// Used when every pooled client is busy (playlist and key requests).
    shared: reqwest::Client,
    max_idle: usize,
}

impl ClientPool {
    fn lease(&self) -> Lease<'_> {
        let pooled = self.idle.lock().pop();
        let returnable = pooled.is_some();
        Lease { pool: self, client: Some(pooled.unwrap_or_else(|| self.shared.clone())), returnable }
    }
}

struct Lease<'a> {
    pool: &'a ClientPool,
    client: Option<reqwest::Client>,
    returnable: bool,
}

impl std::ops::Deref for Lease<'_> {
    type Target = reqwest::Client;

    fn deref(&self) -> &reqwest::Client {
        self.client.as_ref().expect("present until dropped")
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.client.take()
            && self.returnable
        {
            let mut idle = self.pool.idle.lock();
            if idle.len() < self.pool.max_idle {
                idle.push(c);
            }
        }
    }
}

/// Aborts the task when dropped (an abandoned download stops its fetches).
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> Future for AbortOnDrop<T> {
    type Output = std::result::Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

struct ActiveGuard<'a>(&'a AtomicUsize);

impl<'a> ActiveGuard<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::BlockModeEncrypt;

    fn base() -> Url {
        Url::parse("https://cdn.example.com/video/hls/master.m3u8?token=1").unwrap()
    }

    fn media(text: &str) -> MediaPlaylist {
        match parse_playlist(text, &base()).unwrap() {
            Playlist::Media(m) => m,
            Playlist::Master(_) => panic!("expected a media playlist"),
        }
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

    #[test]
    fn attribute_lists() {
        let a = Attributes::parse(r#"BANDWIDTH=1280000, CODECS="avc1.4d401f,mp4a.40.2",resolution=640x360,NAME="a=b""#);
        assert_eq!(a.get("BANDWIDTH"), Some("1280000"));
        assert_eq!(a.get("CODECS"), Some("avc1.4d401f,mp4a.40.2"));
        assert_eq!(a.get("RESOLUTION"), Some("640x360"));
        assert_eq!(a.get("NAME"), Some("a=b"));
        assert_eq!(a.get("MISSING"), None);
        assert!(Attributes::parse("").0.is_empty());
    }

    #[test]
    fn master_selects_best_variant() {
        let text = "\u{feff}#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",DEFAULT=YES,URI=\"audio/en.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n\
            low/index.m3u8\n\
            #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=9000000,URI=\"iframes.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720\n\
            /abs/720.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"aud\"\n\
            https://other.example.net/1080.m3u8\n\
            #EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=100\n\
            tiny.m3u8\n";
        let Playlist::Master(m) = parse_playlist(text, &base()).unwrap() else { panic!("expected master") };
        assert_eq!(m.variants.len(), 4);
        assert_eq!(m.variants[0].uri.as_str(), "https://cdn.example.com/video/hls/low/index.m3u8");
        assert_eq!(m.variants[1].uri.as_str(), "https://cdn.example.com/abs/720.m3u8");
        assert_eq!(m.variants[3].bandwidth, 100);
        let best = m.best_variant().unwrap();
        assert_eq!(best.uri.as_str(), "https://other.example.net/1080.m3u8");
        assert_eq!(best.resolution, Some((1920, 1080)));
        assert_eq!(best.codecs.as_deref(), Some("avc1.640028,mp4a.40.2"));
        let audio = m.separate_audio(best).unwrap();
        assert_eq!(audio.uri.as_ref().unwrap().as_str(), "https://cdn.example.com/video/hls/audio/en.m3u8");
        assert!(m.separate_audio(&m.variants[0]).is_none());
    }

    #[test]
    fn media_playlist_with_key_rotation() {
        let m = media(
            "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-TARGETDURATION:6\n\
             #EXT-X-MEDIA-SEQUENCE:41\n\
             #EXTINF:5.5,\n\
             s0.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"keys/k1\"\n\
             #EXTINF:6,title, with comma\n\
             s1.ts\n\
             #EXT-X-DISCONTINUITY\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"/k2\",IV=0x1F\n\
             #EXTINF:6.0,\n\
             s2.ts\n\
             #EXT-X-KEY:METHOD=NONE\n\
             #EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n\
             #EXTINF:4.0,\n\
             ../s3.ts\n\
             #EXT-X-ENDLIST\n",
        );
        assert!(m.ended && !m.is_live());
        assert_eq!(m.media_sequence, 41);
        assert_eq!(m.target_duration, Some(6.0));
        assert_eq!(m.segments.len(), 4);
        assert_eq!(m.segments.iter().map(|s| s.sequence).collect::<Vec<_>>(), vec![41, 42, 43, 44]);
        assert!((m.duration() - 21.5).abs() < 1e-9);
        assert_eq!(m.segments[0].encryption, None);
        assert_eq!(
            m.segments[1].encryption,
            Some(Encryption::Aes128 {
                key_uri: Url::parse("https://cdn.example.com/video/hls/keys/k1").unwrap(),
                iv: None
            })
        );
        let mut iv = [0u8; 16];
        iv[15] = 0x1f;
        assert_eq!(
            m.segments[2].encryption,
            Some(Encryption::Aes128 { key_uri: Url::parse("https://cdn.example.com/k2").unwrap(), iv: Some(iv) })
        );
        assert!(m.segments[2].discontinuity && !m.segments[1].discontinuity);
        assert_eq!(m.segments[3].encryption, None);
        assert_eq!(m.segments[3].uri.as_str(), "https://cdn.example.com/video/s3.ts");
        assert_eq!(default_iv(42)[15], 42);
        assert_eq!(default_iv(0x0102)[14..], [1, 2]);
    }

    #[test]
    fn byte_ranges_and_init_section() {
        let m = media(
            "#EXTM3U\n\
             #EXT-X-TARGETDURATION:2\n\
             #EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n\
             #EXTINF:2,\n\
             #EXT-X-BYTERANGE:1000@720\n\
             media.mp4\n\
             #EXTINF:2,\n\
             #EXT-X-BYTERANGE:500\n\
             media.mp4\n\
             #EXT-X-DISCONTINUITY\n\
             #EXT-X-MAP:URI=\"init2.mp4\"\n\
             #EXTINF:2,\n\
             #EXT-X-BYTERANGE:300\n\
             other.mp4\n",
        );
        assert!(m.is_live(), "no ENDLIST");
        let s = &m.segments;
        assert_eq!(s[0].range, Some(ByteRange { offset: 720, length: 1000 }));
        assert_eq!(s[1].range, Some(ByteRange { offset: 1720, length: 500 }));
        // A different resource restarts at 0.
        assert_eq!(s[2].range, Some(ByteRange { offset: 0, length: 300 }));
        let init = s[0].init.as_ref().unwrap();
        assert_eq!(init.uri.as_str(), "https://cdn.example.com/video/hls/init.mp4");
        assert_eq!(init.range, Some(ByteRange { offset: 0, length: 720 }));
        assert!(Arc::ptr_eq(init, s[1].init.as_ref().unwrap()));
        assert_eq!(s[2].init.as_ref().unwrap().range, None);
        assert_eq!(ByteRange { offset: 10, length: 5 }.header_value(), "bytes=10-14");

        let jobs = build_jobs(&m);
        let kinds: Vec<&str> = jobs.iter().map(|j| if matches!(j, Job::Init { .. }) { "init" } else { "seg" }).collect();
        assert_eq!(kinds, ["init", "seg", "seg", "init", "seg"]);
        assert_eq!(exact_total(&jobs), None, "init2.mp4 has no range");
        assert_eq!(output_ext(&m), "mp4");
    }

    #[test]
    fn playlist_type_gap_and_tag_boundaries() {
        let m = media(
            "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MEDIA-SEQUENCE:7\n\
             #EXTINF:1,\na.aac\n#EXT-X-GAP\n#EXTINF:1,\nb.aac\n#EXTINF:1,\nc.aac\n",
        );
        assert!(m.ended, "VOD playlists are complete");
        assert_eq!(m.media_sequence, 7, "MEDIA-SEQUENCE is not mistaken for EXT-X-MEDIA");
        assert_eq!(m.segments.iter().map(|s| s.sequence).collect::<Vec<_>>(), vec![7, 9]);
        assert!(m.segments[1].discontinuity, "a gap is a jump in the timeline");
        assert_eq!(output_ext(&m), "aac");

        let event = media("#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXTINF:1,\na.ts\n");
        assert!(event.is_live());
        assert_eq!(output_ext(&event), "ts");
    }

    #[test]
    fn drm_is_flagged() {
        let m = media(
            "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://key\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
             #EXTINF:1,\na.ts\n",
        );
        assert!(matches!(&m.segments[0].encryption, Some(Encryption::Unsupported { method }) if method == "SAMPLE-AES"));
        assert_eq!(ensure_supported(&m).unwrap_err().to_string(), DRM_UNSUPPORTED);

        let m = media(
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"https://lic\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\"\n\
             #EXTINF:1,\na.ts\n",
        );
        assert!(matches!(m.segments[0].encryption, Some(Encryption::Unsupported { .. })));
        assert!(ensure_supported(&m).is_err());

        let m = media("#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI=\"x\"\n#EXT-X-MAP:URI=\"i.mp4\"\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:1,\na.m4s\n");
        assert_eq!(m.segments[0].encryption, None);
        assert!(ensure_supported(&m).is_err(), "encrypted init section");

        let empty = media("#EXTM3U\n#EXT-X-ENDLIST\n");
        assert!(ensure_supported(&empty).unwrap_err().to_string().contains("no media segments"));
    }

    #[test]
    fn malformed_input() {
        assert!(parse_playlist("<html><body>nope</body></html>", &base()).is_err());
        assert!(parse_playlist("", &base()).is_err());
        assert!(parse_playlist("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128\n#EXTINF:1,\na.ts\n", &base()).is_err());
        assert!(parse_playlist("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0xZZ\n", &base()).is_err());
        assert!(parse_playlist("#EXTM3U\n#EXT-X-BYTERANGE:0@5\n#EXTINF:1,\na.ts\n", &base()).is_err());
        assert!(parse_playlist("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:x\n", &base()).is_err());
        assert!(parse_iv("0x00112233445566778899aabbccddeeff00").is_err());
        assert_eq!(parse_iv("0X00112233445566778899AABBCCDDEEFF").unwrap()[15], 0xff);
        assert_eq!(parse_byterange("10").unwrap(), (10, None));
        assert_eq!(parse_byterange(" 10@20 ").unwrap(), (10, Some(20)));
        assert_eq!(parse_resolution("1920X1080"), Some((1920, 1080)));
        assert_eq!(parse_resolution("wide"), None);
    }

    #[test]
    fn output_names() {
        let u = Url::parse("https://x.example/live/playlist.m3u8?t=1").unwrap();
        assert_eq!(output_name(None, &u, "ts"), "playlist.ts");
        assert_eq!(output_name(Some("  "), &u, "mp4"), "playlist.mp4");
        assert_eq!(output_name(Some("My Show.mp4"), &u, "ts"), "My Show.ts");
        assert_eq!(output_name(Some("My Show.ts"), &u, "ts"), "My Show.ts");
        assert_eq!(output_name(Some("Show.S01E02"), &u, "ts"), "Show.S01E02.ts");
        assert_eq!(output_name(Some("a/b:c?.m3u8"), &u, "ts"), "a_b_c_.ts");
        let root = Url::parse("https://x.example/").unwrap();
        assert_eq!(output_name(None, &root, "ts"), "video.ts");
        assert!(is_hls_url("https://x/a/b.M3U8?x=y.mp4"));
        assert!(!is_hls_url("https://x/a.mp4?list=b.m3u8"));
        assert!(is_hls_content_type("application/vnd.apple.mpegURL; charset=utf-8"));
        assert!(!is_hls_content_type("video/mp2t"));
    }

    #[test]
    fn aes128_round_trip() {
        let key = [7u8; 16];
        let iv = default_iv(5);
        for len in [0usize, 1, 15, 16, 17, 188 * 7] {
            let plain: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
            let enc = encrypt(&key, &iv, &plain);
            assert_eq!(enc.len() % 16, 0);
            assert_eq!(decrypt_aes128(&key, &iv, enc).unwrap(), plain);
        }
        assert!(decrypt_aes128(&key, &iv, vec![0u8; 15]).is_err());
        assert!(decrypt_aes128(&key, &iv, Vec::new()).is_err());
        // A wrong key almost always yields invalid padding.
        let enc = encrypt(&key, &iv, b"hello hls segment");
        assert!(decrypt_aes128(&[8u8; 16], &iv, enc).is_err());
    }
}
