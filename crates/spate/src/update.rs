//! Updates. The version in `crates/spate/Cargo.toml` on the repository's
//! main branch says whether a newer Spate exists. Once that version's release
//! is published, its Windows package is downloaded, checked against the
//! SHA-256 published beside it and moved into the program folder, and the app
//! restarts into it.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;
use reqwest::header::HeaderMap;
use spate_core::Manager;
use spate_core::http::{self, NetConfig};

use crate::app::UiEvent;

/// The version this build was made from.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
pub const PROJECT_URL: &str = "https://github.com/VinZie/spate";
const MAIN_BRANCH: &str = "https://raw.githubusercontent.com/VinZie/spate/main";
const APP_EXE: &str = "spate.exe";
/// Folder in the program folder holding update attempts: the download, the
/// unpacked files and the files they replaced, until a later start removes it.
const STAGING: &str = ".spate-update";

const FIRST_CHECK: Duration = Duration::from_secs(20);
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// How often a switched-off daily check looks whether it was switched on.
const RECHECK_SETTING: Duration = Duration::from_secs(60 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

const MAX_TEXT: usize = 1 << 20;
const MAX_PACKAGE: u64 = 256 << 20;
/// What the package may unpack to, at most.
const MAX_UNPACKED: u64 = 1 << 30;

const CANCELLED: &str = "The update was cancelled.";

/// A published release newer than the running app.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    /// The release's section of `CHANGELOG.md` on main; may be empty.
    pub notes: String,
    /// SHA-256 of the Windows package, from the `.sha256` file published with it.
    sha256: String,
}

#[derive(Debug)]
pub enum Check {
    UpToDate,
    /// Main is at this newer version, but its release is not published yet.
    Unreleased(String),
    Available(Release),
}

/// What an install in progress reports.
pub enum Progress {
    Downloading { done: u64, total: Option<u64> },
    Installing,
    /// The executable to restart into, or why the update stopped.
    Finished(Result<PathBuf, String>),
}

pub fn release_page(version: &str) -> String {
    format!("{PROJECT_URL}/releases/tag/v{version}")
}

fn package_file(version: &str) -> String {
    format!("Spate-{version}-windows-x64.zip")
}

fn asset_url(version: &str, file: &str) -> String {
    format!("{PROJECT_URL}/releases/download/v{version}/{file}")
}

// ---------------------------------------------------------------------------
// Checking
// ---------------------------------------------------------------------------

/// Look for an update shortly after start and then once a day while
/// `check_for_updates` is on, and hand a newer release to the window.
/// Development builds do not look: they are updated from source.
pub fn spawn_daily_check(m: Manager, tx: Sender<UiEvent>, ctx: egui::Context) {
    if cfg!(debug_assertions) {
        return;
    }
    let _ = std::thread::Builder::new().name("spate-update".into()).spawn(move || {
        let mut wait = FIRST_CHECK;
        loop {
            std::thread::sleep(wait);
            let settings = m.settings();
            if !settings.check_for_updates {
                wait = RECHECK_SETTING;
                continue;
            }
            wait = CHECK_EVERY;
            if let Ok(Check::Available(release)) = block_on(check(&settings.net, CURRENT)) {
                if tx.send(UiEvent::UpdateAvailable(release)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        }
    });
}

/// Check now (Help > Check for updates).
pub fn spawn_check(net: NetConfig, ctx: egui::Context) -> Receiver<Result<Check, String>> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("spate-update".into()).spawn(move || {
        let _ = tx.send(block_on(check(&net, CURRENT)));
        ctx.request_repaint();
    });
    rx
}

/// Compare `current` with the version on main, and look for its release.
pub async fn check(net: &NetConfig, current: &str) -> Result<Check, String> {
    let client = client(net)?;
    let work = async {
        let latest = version_on_main(&client).await?;
        if !is_newer(&latest, current) {
            return Ok(Check::UpToDate);
        }
        // Main can be ahead of its release; offer the version once the
        // package is published.
        let package = package_file(&latest);
        let Some(sums) = fetch_text(&client, &asset_url(&latest, &format!("{package}.sha256"))).await? else {
            return Ok(Check::Unreleased(latest));
        };
        let sha256 = parse_checksum(&sums, &package).ok_or_else(|| format!("The checksum published for {package} is malformed."))?;
        let changelog = fetch_text(&client, &format!("{MAIN_BRANCH}/CHANGELOG.md")).await.ok().flatten();
        let notes = changelog.and_then(|c| release_notes(&c, &latest)).unwrap_or_default();
        Ok(Check::Available(Release { version: latest, notes, sha256 }))
    };
    tokio::time::timeout(CHECK_TIMEOUT, work).await.map_err(|_| "GitHub did not answer in time.".to_string())?
}

async fn version_on_main(client: &reqwest::Client) -> Result<String, String> {
    let unreadable = || "The version on the main branch could not be read.".to_string();
    let manifest = fetch_text(client, &format!("{MAIN_BRANCH}/crates/spate/Cargo.toml")).await?.ok_or_else(unreadable)?;
    if let Some(v) = manifest_value(&manifest, "package", "version").and_then(quoted) {
        return Ok(v.to_string());
    }
    if manifest_value(&manifest, "package", "version.workspace") == Some("true") {
        let root = fetch_text(client, &format!("{MAIN_BRANCH}/Cargo.toml")).await?.ok_or_else(unreadable)?;
        if let Some(v) = manifest_value(&root, "workspace.package", "version").and_then(quoted) {
            return Ok(v.to_string());
        }
    }
    Err(unreadable())
}

/// `major.minor.patch`. Versions with a pre-release or build suffix are not
/// offered as updates.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.trim().split('.').map(|p| if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) { p.parse::<u64>().ok() } else { None });
    let v = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(v)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

/// The raw value of `key` in the `[table]` of a Cargo manifest.
fn manifest_value<'a>(manifest: &'a str, table: &str, key: &str) -> Option<&'a str> {
    let header = format!("[{table}]");
    let mut in_table = false;
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_table = line == header;
        } else if in_table
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim());
        }
    }
    None
}

fn quoted(v: &str) -> Option<&str> {
    v.strip_prefix('"')?.strip_suffix('"')
}

/// The digest for `file_name` in a `sha256sum`-style checksum file.
fn parse_checksum(text: &str, file_name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next().map(|n| n.trim_start_matches('*'));
        let valid = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        (valid && name.is_none_or(|n| n == file_name)).then(|| hash.to_ascii_lowercase())
    })
}

/// The body of the `## [version]` section of a Keep a Changelog file.
fn release_notes(changelog: &str, version: &str) -> Option<String> {
    let heading = format!("## [{version}]");
    let mut lines = changelog.lines().skip_while(|l| !l.starts_with(&heading));
    lines.next()?;
    let body: Vec<&str> = lines.take_while(|l| !l.starts_with("## ") && !l.starts_with('[')).collect();
    let notes = body.join("\n").trim().to_string();
    (!notes.is_empty()).then_some(notes)
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

/// Run `f` on a small runtime of the calling thread.
fn block_on<T>(f: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(f)
}

/// A client with the user's proxy and timeouts.
fn client(net: &NetConfig) -> Result<reqwest::Client, String> {
    http::build_client(net).map_err(|e| e.to_string())
}

/// GET `url` without credentials, following redirects over https only.
async fn get(client: &reqwest::Client, url: &str) -> Result<reqwest::Response, String> {
    let url = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    let https_only = |u: &reqwest::Url| {
        if u.scheme() == "https" { Ok(()) } else { Err(spate_core::Error::Other(format!("refused a redirect to {u}"))) }
    };
    http::get(client, &url, &url, &HeaderMap::new(), HeaderMap::new(), 10, https_only).await.map_err(|e| e.to_string())
}

/// The text at `url`; `None` when it does not exist (404).
async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<Option<String>, String> {
    let mut resp = get(client, url).await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("GitHub answered {} for {url}", resp.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if body.len() + chunk.len() > MAX_TEXT {
            return Err(format!("{url} is larger than expected"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(String::from_utf8_lossy(&body).into_owned()))
}

// ---------------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------------

/// The folder this copy can update in place: that of a Windows package
/// (`spate-cli.exe` and `extension\chromium` beside the executable), when
/// Spate may write to it. Otherwise why not.
pub fn install_dir() -> Result<PathBuf, String> {
    if !cfg!(windows) {
        return Err("Spate updates itself on Windows only.".into());
    }
    let dir = crate::sys::exe_dir().ok_or("The program folder could not be found.")?;
    let packaged = dir.join("spate-cli.exe").is_file() && dir.join("extension").join("chromium").join("manifest.json").is_file();
    if !packaged {
        return Err("This copy of Spate was not unpacked from a release package.".into());
    }
    let staging = dir.join(STAGING);
    let probe = staging.join(format!("{}-probe", std::process::id()));
    let writable = std::fs::create_dir_all(&staging).and_then(|_| std::fs::write(&probe, b"")).and_then(|_| std::fs::remove_file(&probe));
    let _ = std::fs::remove_dir(&staging);
    writable.map_err(|e| format!("Spate cannot write to {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Download `release` and install it into `dir` (see [`install_dir`]).
/// Setting `cancel` or dropping the receiver stops the download; once files
/// are being moved, the update runs to the end.
pub fn spawn_install(release: Release, dir: PathBuf, net: NetConfig, cancel: Arc<AtomicBool>, ctx: egui::Context) -> Receiver<Progress> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("spate-update".into()).spawn(move || {
        let send = |p: Progress| {
            let delivered = tx.send(p).is_ok();
            ctx.request_repaint();
            delivered
        };
        let result = fetch_and_install(&release, &dir, &net, &cancel, &send);
        send(Progress::Finished(result));
    });
    rx
}

fn fetch_and_install(release: &Release, dir: &Path, net: &NetConfig, cancel: &AtomicBool, send: &dyn Fn(Progress) -> bool) -> Result<PathBuf, String> {
    let work = work_dir(dir)?;
    let zip = work.join(package_file(&release.version));
    let mut last = Instant::now();
    block_on(download(net, release, &zip, |done, total| {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        if last.elapsed() < PROGRESS_EVERY {
            return true;
        }
        last = Instant::now();
        send(Progress::Downloading { done, total })
    }))?;
    if cancel.load(Ordering::Relaxed) || !send(Progress::Installing) {
        return Err(CANCELLED.into());
    }
    install(&zip, dir, &work)
}

/// A fresh folder for one update attempt. Its name starts with this
/// process's id, so [`clean_up`] in a later instance cannot remove it while
/// this one still uses it.
fn work_dir(dir: &Path) -> Result<PathBuf, String> {
    static ATTEMPT: AtomicU64 = AtomicU64::new(0);
    let n = ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let work = dir.join(STAGING).join(format!("{}-{n}", std::process::id()));
    remove_tree(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("Could not create {}: {e}", work.display()))?;
    Ok(work)
}

/// Download the release's package to `dest` and verify its checksum.
/// `progress` is told the bytes received so far and returns `false` to stop.
async fn download(net: &NetConfig, release: &Release, dest: &Path, mut progress: impl FnMut(u64, Option<u64>) -> bool) -> Result<(), String> {
    let client = client(net)?;
    let mut resp = get(&client, &asset_url(&release.version, &package_file(&release.version))).await?;
    if !resp.status().is_success() {
        return Err(format!("GitHub answered {} for the package.", resp.status()));
    }
    let total = resp.content_length();
    if total.is_some_and(|t| t > MAX_PACKAGE) {
        return Err("The package is larger than expected.".into());
    }
    let write_err = |e: io::Error| format!("Could not write {}: {e}", dest.display());
    let mut file = File::create(dest).map_err(write_err)?;
    let stall = Duration::from_secs(net.read_timeout_secs.max(5));
    let mut done = 0u64;
    loop {
        let chunk = tokio::time::timeout(stall, resp.chunk()).await.map_err(|_| "The download stalled.".to_string())?;
        let Some(chunk) = chunk.map_err(|e| e.to_string())? else { break };
        done += chunk.len() as u64;
        if done > MAX_PACKAGE {
            return Err("The package is larger than expected.".into());
        }
        file.write_all(&chunk).map_err(write_err)?;
        if !progress(done, total) {
            return Err(CANCELLED.into());
        }
    }
    file.sync_all().map_err(write_err)?;
    drop(file);
    if total.is_some_and(|t| t != done) {
        return Err("The download ended early.".into());
    }
    let actual = spate_core::download::sha256_file(dest).map_err(|e| format!("Could not read {}: {e}", dest.display()))?;
    if actual != release.sha256 {
        return Err("The downloaded package does not match the checksum published with it, so it was not installed.".into());
    }
    Ok(())
}

/// Unpack the package `zip` into `work/new`, then move each of its files into
/// `dir`, the file it replaces going to `work/old`. Windows lets a running
/// executable be moved, though not overwritten or deleted, so this works for
/// spate.exe itself and for a spate-cli.exe a browser has started. If a file
/// cannot be moved, the ones moved so far are put back. Files of the old
/// version that the package does not contain are left alone. Returns the
/// executable to start.
pub fn install(zip: &Path, dir: &Path, work: &Path) -> Result<PathBuf, String> {
    let (new, old) = (work.join("new"), work.join("old"));
    let files = unpack(zip, &new)?;
    if !files.iter().any(|f| f == Path::new(APP_EXE)) {
        return Err(format!("The package does not contain {APP_EXE}."));
    }
    let mut moved: Vec<(&Path, bool)> = Vec::new();
    for rel in &files {
        match swap_in(dir, &new, &old, rel) {
            Ok(replaced) => moved.push((rel.as_path(), replaced)),
            Err(e) => {
                put_back(dir, &new, &old, &moved);
                return Err(format!("Could not replace {}: {e}", dir.join(rel).display()));
            }
        }
    }
    Ok(dir.join(APP_EXE))
}

/// Extract the package into `into` without its top-level folder. Returns
/// the files' paths relative to `into`.
fn unpack(zip: &Path, into: &Path) -> Result<Vec<PathBuf>, String> {
    let bad = |e: &dyn std::fmt::Display| format!("The package could not be unpacked: {e}");
    let mut archive = zip::ZipArchive::new(File::open(zip).map_err(|e| bad(&e))?).map_err(|e| bad(&e))?;
    let mut files = Vec::new();
    let mut budget = MAX_UNPACKED;
    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(|e| bad(&e))?;
        let name = entry.name().map_err(|e| bad(&e))?.into_owned();
        let rel = package_path(&name).ok_or_else(|| bad(&format!("unexpected path {name}")))?;
        if entry.is_dir() || rel.as_os_str().is_empty() {
            continue;
        }
        let dest = into.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| bad(&e))?;
        }
        let mut out = File::create(&dest).map_err(|e| bad(&e))?;
        let n = io::copy(&mut entry.take(budget + 1), &mut out).map_err(|e| bad(&e))?;
        budget = budget.checked_sub(n).ok_or_else(|| bad(&"it is larger than expected"))?;
        files.push(rel);
    }
    Ok(files)
}

/// A package entry's path inside the package folder (`Spate-<version>-…/`),
/// or `None` if it could land outside that folder.
fn package_path(name: &str) -> Option<PathBuf> {
    if name.starts_with(['/', '\\']) {
        return None;
    }
    let mut parts = name.split(['/', '\\']).filter(|p| !p.is_empty());
    if !parts.next()?.starts_with("Spate-") {
        return None;
    }
    let mut rel = PathBuf::new();
    for part in parts {
        // `:` would name a drive or an alternate data stream on Windows.
        if part == "." || part == ".." || part.contains(':') || part.chars().any(char::is_control) {
            return None;
        }
        rel.push(part);
    }
    Some(rel)
}

/// Move `dir/rel` aside to `old/rel` and `new/rel` into its place. Whether
/// there was a file to replace.
fn swap_in(dir: &Path, new: &Path, old: &Path, rel: &Path) -> io::Result<bool> {
    let (target, backup) = (dir.join(rel), old.join(rel));
    let replaced = std::fs::symlink_metadata(&target).is_ok();
    if replaced {
        create_parent(&backup)?;
        std::fs::rename(&target, &backup)?;
    }
    let placed = create_parent(&target).and_then(|_| std::fs::rename(new.join(rel), &target));
    if let Err(e) = placed {
        if replaced {
            let _ = std::fs::rename(&backup, &target);
        }
        return Err(e);
    }
    Ok(replaced)
}

/// Undo [`swap_in`] for `moved`, newest first.
fn put_back(dir: &Path, new: &Path, old: &Path, moved: &[(&Path, bool)]) {
    for (rel, replaced) in moved.iter().rev() {
        let target = dir.join(rel);
        let _ = std::fs::rename(&target, new.join(rel));
        if *replaced {
            let _ = std::fs::rename(old.join(rel), &target);
        }
    }
}

fn create_parent(path: &Path) -> io::Result<()> {
    path.parent().map_or(Ok(()), std::fs::create_dir_all)
}

/// Remove what earlier updates left in the program folder: the files they
/// replaced and unfinished downloads. A file still in use (an old
/// spate-cli.exe a browser keeps running) stays for a later start.
pub fn clean_up() {
    let Some(staging) = crate::sys::exe_dir().map(|d| d.join(STAGING)) else { return };
    let Ok(entries) = std::fs::read_dir(&staging) else { return };
    let ours = format!("{}-", std::process::id());
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(&ours) {
            remove_tree(&entry.path());
        }
    }
    let _ = std::fs::remove_dir(&staging);
}

/// Delete `path` and what it contains as far as possible, without following
/// links.
fn remove_tree(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return };
    if meta.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            entries.flatten().for_each(|e| remove_tree(&e.path()));
        }
        let _ = std::fs::remove_dir(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("spate-update-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_zip(path: &Path, entries: &[(&str, &str)]) {
        let mut w = zip::ZipWriter::new(File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, contents) in entries {
            if name.ends_with('/') {
                w.add_directory(*name, options).unwrap();
            } else {
                w.start_file(*name, options).unwrap();
                w.write_all(contents.as_bytes()).unwrap();
            }
        }
        w.finish().unwrap();
    }

    fn read(path: impl AsRef<Path>) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.3.0-beta.1", "0.2.0"), "pre-releases are not offered");
        assert!(!is_newer("0.3", "0.2.0"));
        assert!(!is_newer("0.3.0.1", "0.2.0"));
        assert!(!is_newer("+1.0.0", "0.2.0"));
        assert!(!is_newer("", "0.2.0"));
    }

    #[test]
    fn version_comes_from_the_package_table() {
        let manifest = "[package]\nname = \"spate\"\nversion = \"0.2.0\" # bumped\nedition = \"2024\"\n\n[dependencies]\nversion = \"9.9.9\"\n";
        assert_eq!(manifest_value(manifest, "package", "version").and_then(quoted), Some("0.2.0"));
        let deps_first = "[dependencies]\nversion = \"9.9.9\"\n[package]\nversion = \"0.3.0\"\n";
        assert_eq!(manifest_value(deps_first, "package", "version").and_then(quoted), Some("0.3.0"));
        let inherited = "[package]\nname = \"spate\"\nversion.workspace = true\n";
        assert_eq!(manifest_value(inherited, "package", "version"), None);
        assert_eq!(manifest_value(inherited, "package", "version.workspace"), Some("true"));
        let root = "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.4.0\"\n";
        assert_eq!(manifest_value(root, "workspace.package", "version").and_then(quoted), Some("0.4.0"));
    }

    #[test]
    fn checksum_file_names_the_package() {
        let hash = "AB".repeat(32);
        let name = "Spate-0.2.0-windows-x64.zip";
        assert_eq!(parse_checksum(&format!("{hash}  {name}"), name), Some(hash.to_ascii_lowercase()));
        assert_eq!(parse_checksum(&format!("{hash} *{name}\n"), name).as_deref(), Some(&*hash.to_ascii_lowercase()));
        assert_eq!(parse_checksum(&hash, name).as_deref(), Some(&*hash.to_ascii_lowercase()));
        assert_eq!(parse_checksum(&format!("{hash}  other.zip"), name), None);
        assert_eq!(parse_checksum(&format!("{}  {name}", "ab".repeat(31)), name), None);
        assert_eq!(parse_checksum(&format!("{}  {name}", "zz".repeat(32)), name), None);
    }

    #[test]
    fn release_notes_are_the_versions_section() {
        let changelog = "# Changelog\n\nIntro.\n\n## [0.2.0] - 2026-11-01\n\n### Added\n\n- Updates.\n\n## [0.1.0] - 2026-10-24\n\nFirst.\n\n[0.2.0]: https://x\n[0.1.0]: https://y\n";
        assert_eq!(release_notes(changelog, "0.2.0").as_deref(), Some("### Added\n\n- Updates."));
        assert_eq!(release_notes(changelog, "0.1.0").as_deref(), Some("First."));
        assert_eq!(release_notes(changelog, "0.3.0"), None);
        assert_eq!(release_notes("## [0.2.0]\n\n## [0.1.0]\n", "0.2.0"), None);
    }

    #[test]
    fn package_paths_stay_inside_the_package() {
        assert_eq!(package_path("Spate-0.2.0-windows-x64/spate.exe"), Some(PathBuf::from("spate.exe")));
        assert_eq!(package_path("Spate-0.2.0-windows-x64\\extension\\chromium\\manifest.json"), Some(["extension", "chromium", "manifest.json"].iter().collect()));
        assert_eq!(package_path("Spate-0.2.0-windows-x64/"), Some(PathBuf::new()));
        for bad in [
            "spate.exe",
            "other/spate.exe",
            "/Spate-0.2.0/spate.exe",
            "\\Spate-0.2.0\\spate.exe",
            "Spate-0.2.0/../evil.exe",
            "Spate-0.2.0/a/../../evil.exe",
            "Spate-0.2.0/C:/evil.exe",
            "Spate-0.2.0/spate.exe:stream",
            "Spate-0.2.0/./spate.exe",
        ] {
            assert_eq!(package_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn install_moves_the_package_in_and_keeps_what_it_replaced() {
        let t = TempDir::new("install");
        let (dir, work) = (t.0.join("app"), t.0.join("app").join(STAGING).join("1-0"));
        std::fs::create_dir_all(dir.join("extension").join("chromium")).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(dir.join("spate.exe"), "old app").unwrap();
        std::fs::write(dir.join("extension").join("chromium").join("manifest.json"), "old manifest").unwrap();
        std::fs::write(dir.join("notes.txt"), "the user's").unwrap();
        let zip = work.join("package.zip");
        write_zip(&zip, &[
            ("Spate-0.2.0-windows-x64/", ""),
            ("Spate-0.2.0-windows-x64/spate.exe", "new app"),
            ("Spate-0.2.0-windows-x64/spate-cli.exe", "new cli"),
            ("Spate-0.2.0-windows-x64/extension/chromium/manifest.json", "new manifest"),
        ]);

        assert_eq!(install(&zip, &dir, &work).unwrap(), dir.join("spate.exe"));
        assert_eq!(read(dir.join("spate.exe")), "new app");
        assert_eq!(read(dir.join("spate-cli.exe")), "new cli");
        assert_eq!(read(dir.join("extension").join("chromium").join("manifest.json")), "new manifest");
        assert_eq!(read(dir.join("notes.txt")), "the user's");
        assert_eq!(read(work.join("old").join("spate.exe")), "old app");
        assert_eq!(read(work.join("old").join("extension").join("chromium").join("manifest.json")), "old manifest");
        assert!(!work.join("old").join("spate-cli.exe").exists(), "nothing was there to keep");
    }

    #[test]
    fn failed_install_puts_everything_back() {
        let t = TempDir::new("rollback");
        let (dir, work) = (t.0.join("app"), t.0.join("work"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(dir.join("spate.exe"), "old app").unwrap();
        std::fs::write(dir.join("README.txt"), "old readme").unwrap();
        // A file where the package needs a folder: the third move fails.
        std::fs::write(dir.join("extension"), "in the way").unwrap();
        let zip = work.join("package.zip");
        write_zip(&zip, &[
            ("Spate-0.2.0-windows-x64/spate.exe", "new app"),
            ("Spate-0.2.0-windows-x64/README.txt", "new readme"),
            ("Spate-0.2.0-windows-x64/extension/chromium/manifest.json", "new manifest"),
        ]);

        let err = install(&zip, &dir, &work).unwrap_err();
        assert!(err.contains("manifest.json"), "{err}");
        assert_eq!(read(dir.join("spate.exe")), "old app");
        assert_eq!(read(dir.join("README.txt")), "old readme");
        assert_eq!(read(dir.join("extension")), "in the way");
    }

    #[test]
    fn packages_that_escape_or_lack_the_app_are_refused() {
        let t = TempDir::new("refuse");
        let (dir, work) = (t.0.join("app"), t.0.join("work"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(dir.join("spate.exe"), "old app").unwrap();

        let zip = work.join("escape.zip");
        write_zip(&zip, &[("Spate-0.2.0-windows-x64/spate.exe", "new app"), ("Spate-0.2.0-windows-x64/../../evil.txt", "evil")]);
        assert!(install(&zip, &dir, &work).unwrap_err().contains("unexpected path"));
        assert!(!t.0.join("evil.txt").exists() && !dir.join("evil.txt").exists());
        assert_eq!(read(dir.join("spate.exe")), "old app");

        let zip = work.join("no-app.zip");
        write_zip(&zip, &[("Spate-0.2.0-windows-x64/README.txt", "readme")]);
        assert!(install(&zip, &dir, &work).unwrap_err().contains("spate.exe"));
        assert_eq!(read(dir.join("spate.exe")), "old app");
        assert!(!dir.join("README.txt").exists());
    }
}
