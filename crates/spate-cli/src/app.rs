//! Talking to the Spate desktop app over its token-authenticated loopback
//! IPC, launching it on demand.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::Args;
use spate_core::ipc::{self, AddItem, Request};
use spate_core::{StartMode, paths};

use crate::{EXIT_ERROR, EXIT_OK, options};

/// How long to wait for a freshly launched app to answer.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Args, Debug)]
pub struct AddArgs {
    #[arg(required = true, value_name = "URL")]
    pub urls: Vec<String>,
    /// Folder to save into [default: the app's folder for the file type]
    #[arg(short, long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// File name to save as (single URL only)
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,
    /// Add to the queue instead of starting right away
    #[arg(short, long)]
    pub queue: bool,
    /// Don't show the app's confirmation dialog
    #[arg(short, long)]
    pub silent: bool,
    /// Referer to send
    #[arg(long, alias = "referrer", value_name = "URL")]
    pub referer: Option<String>,
    /// Cookie header value
    #[arg(long, value_name = "COOKIES")]
    pub cookie: Option<String>,
    /// User-Agent to send
    #[arg(long, value_name = "UA")]
    pub user_agent: Option<String>,
    /// Extra request header (repeatable)
    #[arg(short = 'H', long = "header", value_name = "NAME: VALUE", value_parser = options::parse_header)]
    pub headers: Vec<(String, String)>,
}

/// Path of the desktop app: `SPATE_APP_EXE`, else `spate(.exe)` next to
/// this executable.
pub fn app_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SPATE_APP_EXE") {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) { "spate.exe" } else { "spate" };
    Some(exe.with_file_name(name))
}

/// Start the app in the background, fully detached from this process (and
/// from the browser's job object when we run as the native host).
pub fn launch_app(exe: &Path) -> io::Result<()> {
    let mut cmd = Command::new(exe);
    cmd.arg("--background").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            Ok(_) => return Ok(()),
            // The job we are in may forbid breakaway (access denied).
            Err(_) => {
                cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map(drop)
}

/// Make sure an app instance answers on `data_dir`'s endpoint, launching it
/// if needed. `log` receives progress notes.
pub fn ensure_running(data_dir: &Path, log: &mut dyn FnMut(&str)) -> Result<(), String> {
    if ipc::is_running(data_dir) {
        return Ok(());
    }
    let exe = app_exe().ok_or("cannot locate the Spate app")?;
    if !exe.is_file() {
        return Err(format!("Spate is not running and {} was not found", exe.display()));
    }
    log(&format!("starting {}", exe.display()));
    launch_app(&exe).map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        if ipc::is_running(data_dir) {
            return Ok(());
        }
    }
    Err(format!("Spate did not respond within {} s after starting", STARTUP_TIMEOUT.as_secs()))
}

/// Send `items` to the app (launching it if needed).
pub fn send_add(
    data_dir: &Path,
    items: Vec<AddItem>,
    interactive: bool,
    start: Option<StartMode>,
    log: &mut dyn FnMut(&str),
) -> Result<(), String> {
    ensure_running(data_dir, log)?;
    let resp =
        ipc::send(data_dir, &Request::Add { items, interactive, start }).map_err(|e| format!("IPC failed: {e}"))?;
    if resp.ok { Ok(()) } else { Err(resp.error.unwrap_or_else(|| "the app rejected the request".into())) }
}

pub fn build_items(args: &AddArgs) -> Result<Vec<AddItem>, String> {
    if args.urls.len() > 1 && args.name.is_some() {
        return Err("--name can only be used with a single URL".into());
    }
    let dir = match &args.dir {
        Some(d) => Some(std::path::absolute(d).map_err(|e| format!("invalid folder: {e}"))?),
        None => None,
    };
    let clean = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    args.urls
        .iter()
        .map(|u| {
            let parsed = url::Url::parse(u.trim()).map_err(|e| format!("invalid URL `{u}`: {e}"))?;
            Ok(AddItem {
                url: parsed.to_string(),
                file_name: clean(&args.name),
                referrer: clean(&args.referer),
                cookies: clean(&args.cookie),
                user_agent: clean(&args.user_agent),
                headers: args.headers.clone(),
                dir: dir.clone(),
                ..Default::default()
            })
        })
        .collect()
}

pub fn run_add(args: AddArgs) -> u8 {
    let items = match build_items(&args) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_ERROR;
        }
    };
    let start = args.queue.then_some(StartMode::Queue);
    let n = items.len();
    let data_dir = paths::data_dir();
    match send_add(&data_dir, items, !args.silent, start, &mut |m| eprintln!("{m}")) {
        Ok(()) => {
            eprintln!("Sent {n} download{} to Spate", if n == 1 { "" } else { "s" });
            EXIT_OK
        }
        Err(e) => {
            eprintln!("error: {e}");
            EXIT_ERROR
        }
    }
}

pub fn run_show() -> u8 {
    let data_dir = paths::data_dir();
    let result = ensure_running(&data_dir, &mut |m| eprintln!("{m}"))
        .and_then(|()| ipc::send(&data_dir, &Request::Show).map_err(|e| format!("IPC failed: {e}")))
        .and_then(|r| if r.ok { Ok(()) } else { Err(r.error.unwrap_or_default()) });
    match result {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("error: {e}");
            EXIT_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn items_from_args() {
        let args = AddArgs {
            urls: vec!["https://example.com/a b.zip".into()],
            dir: Some(PathBuf::from("dl")),
            name: Some(" x.zip ".into()),
            queue: false,
            silent: true,
            referer: Some("https://example.com/".into()),
            cookie: Some("".into()),
            user_agent: None,
            headers: vec![("X-A".into(), "1".into())],
        };
        let items = build_items(&args).unwrap();
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.url, "https://example.com/a%20b.zip");
        assert_eq!(it.file_name.as_deref(), Some("x.zip"));
        assert_eq!(it.cookies, None);
        assert!(it.dir.as_ref().unwrap().is_absolute());
        assert!(it.request_headers().contains(&("Referer".into(), "https://example.com/".into())));

        let two = AddArgs { urls: vec!["https://a/1".into(), "https://a/2".into()], ..args };
        assert!(build_items(&two).is_err(), "--name needs a single URL");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_reaches_running_app() {
        let dir = std::env::temp_dir().join(format!("spate-cli-ipc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let seen: Arc<Mutex<Vec<Request>>> = Arc::default();
        let s = seen.clone();
        let handler: ipc::Handler = Arc::new(move |req| {
            s.lock().unwrap().push(req);
            ipc::Response::ok()
        });
        let server = ipc::serve(&dir, handler).await.unwrap();
        let d = dir.clone();
        let item = AddItem { url: "https://example.com/f".into(), ..Default::default() };
        let sent = item.clone();
        tokio::task::spawn_blocking(move || send_add(&d, vec![sent], false, None, &mut |_| {}))
            .await
            .unwrap()
            .unwrap();
        let got = seen.lock().unwrap().clone();
        assert!(got.contains(&Request::Add { items: vec![item], interactive: false, start: None }));
        drop(server);
        std::fs::remove_dir_all(&dir).ok();
    }
}
