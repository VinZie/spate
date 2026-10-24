//! Browser registration of the native-messaging host (per user, no admin
//! rights needed) and the `status` report.
//!
//! Manifests live in `<data dir>/native-host/`; on Windows each browser finds
//! them through a `HKCU\...\NativeMessagingHosts\com.spate.host` key whose
//! default value is the manifest path. Google Chrome reads its own key and
//! falls back to Chromium's; Chromium-branded forks (Brave, Vivaldi, Opera)
//! read Chromium's and fall back to Chrome's; Edge and Firefox use their own.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};
use spate_core::ipc::{self, NATIVE_HOST_NAME};
use spate_core::paths;
use serde_json::json;

use crate::{EXIT_ERROR, EXIT_OK, app};

pub const MANIFEST_DIR: &str = "native-host";
pub const CHROMIUM_MANIFEST: &str = "com.spate.host.chromium.json";
pub const FIREFOX_MANIFEST: &str = "com.spate.host.firefox.json";
const DESCRIPTION: &str = "Spate download manager browser integration";

const CHROME_KEY: &str = r"Software\Google\Chrome\NativeMessagingHosts";
const CHROMIUM_KEY: &str = r"Software\Chromium\NativeMessagingHosts";
const EDGE_KEY: &str = r"Software\Microsoft\Edge\NativeMessagingHosts";
const BRAVE_KEY: &str = r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts";
const MOZILLA_KEY: &str = r"Software\Mozilla\NativeMessagingHosts";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Browser {
    Chrome,
    Edge,
    Brave,
    Chromium,
    Vivaldi,
    Opera,
    Firefox,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    Chromium,
    Firefox,
}

impl Browser {
    const EACH: [Browser; 7] = [
        Browser::Chrome,
        Browser::Edge,
        Browser::Brave,
        Browser::Chromium,
        Browser::Vivaldi,
        Browser::Opera,
        Browser::Firefox,
    ];

    pub fn family(self) -> Family {
        if self == Browser::Firefox { Family::Firefox } else { Family::Chromium }
    }

    /// Registry keys (under HKCU) this browser consults.
    pub fn keys(self) -> &'static [&'static str] {
        match self {
            Browser::Chrome => &[CHROME_KEY],
            Browser::Edge => &[EDGE_KEY],
            Browser::Brave => &[CHROMIUM_KEY, BRAVE_KEY],
            Browser::Chromium | Browser::Vivaldi | Browser::Opera => &[CHROMIUM_KEY],
            Browser::Firefox => &[MOZILLA_KEY],
            Browser::All => &[],
        }
    }
}

/// Every key we may ever write, with the browsers that read it.
const ALL_KEYS: &[(&str, &str, Family)] = &[
    (CHROME_KEY, "Chrome (and fallback for Chromium forks)", Family::Chromium),
    (CHROMIUM_KEY, "Chromium, Brave, Vivaldi, Opera (and fallback for Chrome)", Family::Chromium),
    (EDGE_KEY, "Microsoft Edge", Family::Chromium),
    (BRAVE_KEY, "Brave", Family::Chromium),
    (MOZILLA_KEY, "Firefox", Family::Firefox),
];

#[derive(Args, Debug)]
pub struct InstallArgs {
    /// Browsers to register with (comma separated)
    #[arg(short, long, value_enum, value_delimiter = ',', default_value = "all")]
    pub browser: Vec<Browser>,
    /// Extra extension allowed to connect (repeatable): a 32-letter Chromium
    /// extension ID or a Firefox add-on ID such as name@example.org
    #[arg(long = "extension-id", value_name = "ID")]
    pub extension_ids: Vec<String>,
}

pub fn manifest_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(MANIFEST_DIR)
}

pub fn manifest_path(data_dir: &Path, family: Family) -> PathBuf {
    manifest_dir(data_dir).join(match family {
        Family::Chromium => CHROMIUM_MANIFEST,
        Family::Firefox => FIREFOX_MANIFEST,
    })
}

pub fn is_chromium_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

pub fn is_firefox_id(id: &str) -> bool {
    let guid = id.len() > 2 && id.starts_with('{') && id.ends_with('}');
    let email = id.split_once('@').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty());
    !id.contains(char::is_whitespace) && (guid || email)
}

/// Split extra IDs into (Chromium, Firefox), defaults first, deduplicated.
pub fn sort_ids(extra: &[String]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut chromium = vec![ipc::CHROMIUM_EXTENSION_ID.to_string()];
    let mut firefox = vec![ipc::FIREFOX_EXTENSION_ID.to_string()];
    for id in extra.iter().map(|s| s.trim()) {
        let id = id.strip_prefix("chrome-extension://").map(|r| r.trim_end_matches('/')).unwrap_or(id);
        let list = if is_chromium_id(id) {
            &mut chromium
        } else if is_firefox_id(id) {
            &mut firefox
        } else {
            return Err(format!("`{id}` is neither a Chromium extension ID nor a Firefox add-on ID"));
        };
        if !list.iter().any(|x| x == id) {
            list.push(id.to_string());
        }
    }
    Ok((chromium, firefox))
}

pub fn chromium_manifest(host: &Path, ids: &[String]) -> serde_json::Value {
    let origins: Vec<String> = ids.iter().map(|id| format!("chrome-extension://{id}/")).collect();
    json!({
        "name": NATIVE_HOST_NAME,
        "description": DESCRIPTION,
        "path": host.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": origins,
    })
}

pub fn firefox_manifest(host: &Path, ids: &[String]) -> serde_json::Value {
    json!({
        "name": NATIVE_HOST_NAME,
        "description": DESCRIPTION,
        "path": host.to_string_lossy(),
        "type": "stdio",
        "allowed_extensions": ids,
    })
}

fn read_manifest(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Extension IDs listed in the installed manifests: (Chromium, Firefox).
pub fn manifest_ids(data_dir: &Path) -> (Vec<String>, Vec<String>) {
    let strings = |v: Option<&serde_json::Value>| -> Vec<String> {
        v.and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let chromium = read_manifest(&manifest_path(data_dir, Family::Chromium))
        .map(|m| strings(m.get("allowed_origins")))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|o| o.strip_prefix("chrome-extension://").map(|r| r.trim_end_matches('/').to_string()))
        .collect();
    let firefox = read_manifest(&manifest_path(data_dir, Family::Firefox))
        .map(|m| strings(m.get("allowed_extensions")))
        .unwrap_or_default();
    (chromium, firefox)
}

fn selected(browsers: &[Browser]) -> BTreeSet<Browser> {
    if browsers.is_empty() || browsers.contains(&Browser::All) {
        Browser::EACH.into_iter().collect()
    } else {
        browsers.iter().copied().collect()
    }
}

pub fn run_install(args: InstallArgs) -> u8 {
    match install(&args) {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("error: {e}");
            EXIT_ERROR
        }
    }
}

fn install(args: &InstallArgs) -> Result<(), String> {
    let (chromium_ids, firefox_ids) = sort_ids(&args.extension_ids)?;
    let host = std::env::current_exe().map_err(|e| format!("cannot locate this executable: {e}"))?;
    let data_dir = paths::data_dir();
    let browsers = selected(&args.browser);
    let families: BTreeSet<Family> = browsers.iter().map(|b| b.family()).collect();

    for family in &families {
        let manifest = match family {
            Family::Chromium => chromium_manifest(&host, &chromium_ids),
            Family::Firefox => firefox_manifest(&host, &firefox_ids),
        };
        let path = manifest_path(&data_dir, *family);
        let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
        paths::write_atomic(&path, &bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        println!("Wrote {}", path.display());
    }

    let mut keys: BTreeSet<(&str, Family)> = BTreeSet::new();
    for b in &browsers {
        for k in b.keys() {
            keys.insert((k, b.family()));
        }
    }
    let mut created = read_created_keys(&data_dir);
    let mut result = Ok(());
    for (key, family) in keys {
        let manifest = manifest_path(&data_dir, family);
        match reg::set_host(key, &manifest) {
            Ok(new_keys) => {
                created.extend(new_keys);
                println!("Registered HKCU\\{key}\\{NATIVE_HOST_NAME}");
            }
            Err(e) => {
                result = Err(format!("cannot write HKCU\\{key}: {e}"));
                break;
            }
        }
    }
    // Remember which parent keys did not exist so uninstall leaves no trace.
    if !created.is_empty() {
        let json = serde_json::to_vec_pretty(&created).map_err(|e| e.to_string())?;
        paths::write_atomic(&created_keys_path(&data_dir), &json).map_err(|e| e.to_string())?;
    }
    result?;
    let names: Vec<String> = browsers.iter().map(|b| format!("{b:?}").to_lowercase()).collect();
    println!("Native host {} -> {} ({})", NATIVE_HOST_NAME, host.display(), names.join(", "));
    Ok(())
}

pub fn run_uninstall() -> u8 {
    let data_dir = paths::data_dir();
    let mut failed = false;
    for (key, _, _) in ALL_KEYS {
        match reg::remove_host(key) {
            Ok(true) => println!("Removed HKCU\\{key}\\{NATIVE_HOST_NAME}"),
            Ok(false) => {}
            Err(e) => {
                failed = true;
                eprintln!("error: cannot remove HKCU\\{key}\\{NATIVE_HOST_NAME}: {e}");
            }
        }
    }
    for family in [Family::Chromium, Family::Firefox] {
        let path = manifest_path(&data_dir, family);
        if path.exists() {
            match std::fs::remove_file(&path) {
                Ok(()) => println!("Deleted {}", path.display()),
                Err(e) => {
                    failed = true;
                    eprintln!("error: cannot delete {}: {e}", path.display());
                }
            }
        }
    }
    // Parent keys that install-host had to create, deepest first.
    let mut created: Vec<String> = read_created_keys(&data_dir).into_iter().collect();
    created.sort_by_key(|k| std::cmp::Reverse(k.matches('\\').count()));
    for key in &created {
        match reg::remove_if_empty(key) {
            Ok(true) => println!("Removed HKCU\\{key} (created by install-host)"),
            Ok(false) => {}
            Err(e) => eprintln!("warning: cannot remove HKCU\\{key}: {e}"),
        }
    }
    let _ = std::fs::remove_file(created_keys_path(&data_dir));
    let _ = std::fs::remove_dir(manifest_dir(&data_dir)); // only if empty
    if failed { EXIT_ERROR } else { EXIT_OK }
}

fn created_keys_path(data_dir: &Path) -> PathBuf {
    manifest_dir(data_dir).join("created-keys.json")
}

fn read_created_keys(data_dir: &Path) -> BTreeSet<String> {
    std::fs::read(created_keys_path(data_dir))
        .ok()
        .and_then(|b| serde_json::from_slice::<BTreeSet<String>>(&b).ok())
        .unwrap_or_default()
}

pub fn run_status() -> u8 {
    let data_dir = paths::data_dir();
    println!("Spate CLI {}", ipc::VERSION);
    println!("Data folder:  {}", data_dir.display());
    match ipc::read_info(&data_dir) {
        Some(info) if ipc::is_running(&data_dir) => {
            println!("App:          running (pid {}, IPC port {})", info.pid, info.port)
        }
        Some(_) => println!("App:          not running (stale endpoint file)"),
        None => println!("App:          not running"),
    }
    if let Some(exe) = app::app_exe() {
        let state = if exe.is_file() { "found" } else { "missing" };
        println!("App program:  {} ({state})", exe.display());
    }
    let host = std::env::current_exe().ok();
    if let Some(h) = &host {
        println!("Host program: {}", h.display());
    }

    println!("Manifests:");
    for family in [Family::Chromium, Family::Firefox] {
        let path = manifest_path(&data_dir, family);
        let label = if family == Family::Chromium { "chromium" } else { "firefox" };
        match read_manifest(&path) {
            Some(m) => {
                let target = m.get("path").and_then(|p| p.as_str()).unwrap_or("");
                let allowed = m.get("allowed_origins").or_else(|| m.get("allowed_extensions")).cloned().unwrap_or_default();
                let note = match &host {
                    Some(h) if !same_path(Path::new(target), h) => format!(" [host is {target}, not this program]"),
                    _ if !Path::new(target).is_file() => " [host program missing]".to_string(),
                    _ => String::new(),
                };
                println!("  {label:<9} {}{note}\n            allowed: {allowed}", path.display());
            }
            None => println!("  {label:<9} not installed"),
        }
    }

    println!("Browser registrations (HKCU):");
    for (key, browsers, family) in ALL_KEYS {
        let line = match reg::get_host(key) {
            Ok(Some(value)) => {
                let ours = manifest_path(&data_dir, *family);
                let state = if !Path::new(&value).is_file() {
                    "manifest missing"
                } else if same_path(Path::new(&value), &ours) {
                    "ok"
                } else {
                    "points to another manifest"
                };
                format!("{value} [{state}]")
            }
            Ok(None) => "not registered".into(),
            Err(e) => format!("unavailable: {e}"),
        };
        println!("  {key}\n    ({browsers}): {line}");
    }
    EXIT_OK
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let s = p.to_string_lossy().replace('/', "\\");
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    norm(a) == norm(b)
}

#[cfg(windows)]
mod reg {
    use std::io;
    use std::path::Path;

    use spate_core::ipc::NATIVE_HOST_NAME;
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, REG_CREATED_NEW_KEY};

    fn hkcu() -> RegKey {
        RegKey::predef(HKEY_CURRENT_USER)
    }

    /// Point `HKCU\<parent>\com.spate.host` at `manifest`. Returns the
    /// ancestor keys that did not exist before, so uninstall can remove
    /// exactly those again.
    pub fn set_host(parent: &str, manifest: &Path) -> io::Result<Vec<String>> {
        let root = hkcu();
        let parts: Vec<&str> = parent.split('\\').collect();
        let mut created = Vec::new();
        for depth in 2..=parts.len() {
            let path = parts[..depth].join("\\");
            if let (_, REG_CREATED_NEW_KEY) = root.create_subkey(&path)? {
                created.push(path);
            }
        }
        let (key, _) = root.create_subkey(format!(r"{parent}\{NATIVE_HOST_NAME}"))?;
        key.set_value("", &manifest.to_string_lossy().into_owned())?;
        Ok(created)
    }

    pub fn get_host(parent: &str) -> io::Result<Option<String>> {
        match hkcu().open_subkey_with_flags(format!(r"{parent}\{NATIVE_HOST_NAME}"), KEY_READ) {
            Ok(key) => match key.get_value::<String, _>("") {
                Ok(v) => Ok(Some(v)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Some(String::new())),
                Err(e) => Err(e),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Delete our key. Returns whether it existed.
    pub fn remove_host(parent: &str) -> io::Result<bool> {
        match hkcu().delete_subkey_all(format!(r"{parent}\{NATIVE_HOST_NAME}")) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Delete a key we created earlier, but only if it is still empty.
    pub fn remove_if_empty(path: &str) -> io::Result<bool> {
        let root = hkcu();
        let empty = match root.open_subkey_with_flags(path, KEY_READ) {
            Ok(k) => k.query_info().is_ok_and(|i| i.sub_keys == 0 && i.values == 0),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if empty {
            root.delete_subkey(path)?;
        }
        Ok(empty)
    }
}

#[cfg(not(windows))]
mod reg {
    use std::io;
    use std::path::Path;

    fn unsupported() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "browser registration is only implemented on Windows so far")
    }

    pub fn set_host(_parent: &str, _manifest: &Path) -> io::Result<Vec<String>> {
        Err(unsupported())
    }

    pub fn get_host(_parent: &str) -> io::Result<Option<String>> {
        Err(unsupported())
    }

    pub fn remove_host(_parent: &str) -> io::Result<bool> {
        Ok(false)
    }

    pub fn remove_if_empty(_path: &str) -> io::Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_ids_are_sorted_by_kind() {
        let extra = vec![
            "abcdefghijklmnopabcdefghijklmnop".to_string(),
            "chrome-extension://ponmlkjihgfedcbaponmlkjihgfedcba/".to_string(),
            "dev@example.org".to_string(),
            "{12345678-1234-1234-1234-123456789abc}".to_string(),
            ipc::CHROMIUM_EXTENSION_ID.to_string(),
        ];
        let (c, f) = sort_ids(&extra).unwrap();
        assert_eq!(c, vec![ipc::CHROMIUM_EXTENSION_ID, "abcdefghijklmnopabcdefghijklmnop", "ponmlkjihgfedcbaponmlkjihgfedcba"]);
        assert_eq!(f, vec![ipc::FIREFOX_EXTENSION_ID, "dev@example.org", "{12345678-1234-1234-1234-123456789abc}"]);
        assert!(sort_ids(&["not-an-id".into()]).is_err());
        assert!(sort_ids(&["ABCDEFGHIJKLMNOPABCDEFGHIJKLMNOP".into()]).is_err());
    }

    #[test]
    fn manifests_have_browser_specific_shapes() {
        let host = Path::new(r"C:\Program Files\Spate\spate-cli.exe");
        let c = chromium_manifest(host, &[ipc::CHROMIUM_EXTENSION_ID.into()]);
        assert_eq!(c["name"], NATIVE_HOST_NAME);
        assert_eq!(c["type"], "stdio");
        assert_eq!(c["path"], r"C:\Program Files\Spate\spate-cli.exe");
        assert_eq!(c["allowed_origins"][0], format!("chrome-extension://{}/", ipc::CHROMIUM_EXTENSION_ID));
        assert!(c.get("allowed_extensions").is_none());
        let f = firefox_manifest(host, &[ipc::FIREFOX_EXTENSION_ID.into()]);
        assert_eq!(f["allowed_extensions"][0], ipc::FIREFOX_EXTENSION_ID);
        assert!(f.get("allowed_origins").is_none());
    }

    #[test]
    fn manifest_ids_read_back() {
        let dir = std::env::temp_dir().join(format!("spate-cli-manifest-{}", std::process::id()));
        let host = Path::new("x.exe");
        let ids = vec![ipc::CHROMIUM_EXTENSION_ID.to_string(), "abcdefghijklmnopabcdefghijklmnop".to_string()];
        let m = serde_json::to_vec(&chromium_manifest(host, &ids)).unwrap();
        paths::write_atomic(&manifest_path(&dir, Family::Chromium), &m).unwrap();
        let (c, f) = manifest_ids(&dir);
        assert_eq!(c, ids);
        assert!(f.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Writes under a throw-away HKCU key and removes it again.
    #[cfg(windows)]
    #[test]
    fn registry_roundtrip_leaves_no_trace() {
        let top = format!(r"Software\SpateCliTest{}", std::process::id());
        let parent = format!(r"{top}\NativeMessagingHosts");
        let manifest = Path::new(r"C:\x\com.spate.host.chromium.json");
        let created = reg::set_host(&parent, manifest).unwrap();
        assert_eq!(created, vec![top.clone(), parent.clone()]);
        assert_eq!(reg::get_host(&parent).unwrap().as_deref(), Some(r"C:\x\com.spate.host.chromium.json"));
        assert!(reg::set_host(&parent, manifest).unwrap().is_empty(), "nothing new the second time");
        assert!(!reg::remove_if_empty(&parent).unwrap(), "not empty while our key exists");
        assert!(reg::remove_host(&parent).unwrap());
        assert!(!reg::remove_host(&parent).unwrap());
        assert!(reg::remove_if_empty(&parent).unwrap());
        assert!(reg::remove_if_empty(&top).unwrap());
        assert_eq!(reg::get_host(&parent).unwrap(), None);
        assert!(!reg::remove_if_empty(&top).unwrap(), "already gone");
    }

    #[test]
    fn browser_selection() {
        assert_eq!(selected(&[Browser::All]).len(), 7);
        assert_eq!(selected(&[]).len(), 7);
        let s = selected(&[Browser::Edge, Browser::Firefox]);
        assert!(s.contains(&Browser::Edge) && s.contains(&Browser::Firefox) && s.len() == 2);
        // Every key a browser uses is known to uninstall/status.
        for b in Browser::EACH {
            for k in b.keys() {
                assert!(ALL_KEYS.iter().any(|(key, _, fam)| key == k && *fam == b.family()));
            }
        }
    }
}
