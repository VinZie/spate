//! Operating-system helpers: opening files and folders, helper processes,
//! sign-in startup and power actions.

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A command that does not flash a console window on Windows.
pub fn quiet_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// Open a file with its default application.
pub fn open_path(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        Command::new("explorer.exe").arg(path).spawn().map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(path).spawn().map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open").arg(path).spawn().map(|_| ())
    }
}

/// Show `path` in the file manager, selecting it when it exists.
pub fn reveal(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        if path.is_file() {
            // explorer parses `/select,` itself, so pass the argument verbatim.
            let arg = format!("/select,\"{}\"", path.display());
            Command::new("explorer.exe").raw_arg(arg).spawn().map(|_| ())
        } else {
            let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
            Command::new("explorer.exe").arg(dir).spawn().map(|_| ())
        }
    }
    #[cfg(not(windows))]
    {
        let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        open_path(dir)
    }
}

/// Ask the shell which program should open `path`.
pub fn open_with(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        Command::new("rundll32.exe")
            .raw_arg(format!("shell32.dll,OpenAs_RunDLL {}", path.display()))
            .spawn()
            .map(|_| ())
    }
    #[cfg(not(windows))]
    {
        open_path(path)
    }
}

pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// The command-line companion that registers the browser helper.
pub fn cli_path() -> Option<PathBuf> {
    let name = if cfg!(windows) { "spate-cli.exe" } else { "spate-cli" };
    let p = exe_dir()?.join(name);
    p.exists().then_some(p)
}

/// Run `spate-cli install-host` and return its combined output.
pub fn install_browser_host() -> Result<String, String> {
    let cli = cli_path().ok_or_else(|| "spate-cli was not found next to spate. Build it with `cargo build -p spate-cli`.".to_string())?;
    let out = quiet_command(&cli).arg("install-host").output().map_err(|e| format!("Could not run {}: {e}", cli.display()))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&err);
    }
    if text.trim().is_empty() {
        text = if out.status.success() { "Done.".into() } else { format!("Exited with {}", out.status) };
    }
    if out.status.success() { Ok(text) } else { Err(text) }
}

/// Locate the `extension/` folder: next to the executable for the packaged
/// build, or at the workspace root for development builds.
fn extension_root() -> Option<PathBuf> {
    let exe = exe_dir()?;
    exe.ancestors()
        .take(5)
        .map(|d| d.join("extension"))
        .find(|d| d.join("manifest.json").exists() || d.is_dir())
}

/// The unpacked Chromium extension: `extension/chromium/` in the packaged
/// build, `extension/` itself in a source checkout.
pub fn extension_dir() -> Option<PathBuf> {
    let root = extension_root()?;
    let packaged = root.join("chromium");
    Some(if packaged.join("manifest.json").exists() { packaged } else { root })
}

/// The Firefox build's manifest: `extension/firefox/` when packaged,
/// `extension/dist/firefox/` after `node build.mjs` in a source checkout.
pub fn firefox_manifest() -> Option<PathBuf> {
    let root = extension_root()?;
    [root.join("firefox"), root.join("dist").join("firefox")]
        .into_iter()
        .map(|d| d.join("manifest.json"))
        .find(|p| p.exists())
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const RUN_VALUE: &str = "Spate";

/// Whether Spate is registered to start at sign-in.
pub fn autostart_enabled() -> bool {
    #[cfg(windows)]
    {
        quiet_command("reg.exe")
            .args(["query", RUN_KEY, "/v", RUN_VALUE])
            .output()
            .is_ok_and(|o| o.status.success())
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn set_autostart(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let status = if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let data = format!("\"{}\" --background", exe.display());
            quiet_command("reg.exe").args(["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &data, "/f"]).status()
        } else {
            quiet_command("reg.exe").args(["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]).status()
        };
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!("reg.exe exited with {s}")),
            Err(e) => Err(e.to_string()),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("Starting at sign-in is only supported on Windows.".into())
    }
}

/// Schedule a system shutdown in `secs` seconds (cancellable).
pub fn schedule_shutdown(secs: u32) -> Result<(), String> {
    #[cfg(windows)]
    let r = quiet_command("shutdown.exe")
        .args(["/s", "/t", &secs.to_string(), "/c", "Spate finished all downloads."])
        .status();
    #[cfg(not(windows))]
    let r = Command::new("shutdown").args(["-h", &format!("+{}", secs.div_ceil(60))]).status();
    match r {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("shutdown exited with {s}")),
        Err(e) => Err(e.to_string()),
    }
}

pub fn cancel_shutdown() {
    #[cfg(windows)]
    let _ = quiet_command("shutdown.exe").arg("/a").status();
    #[cfg(not(windows))]
    let _ = Command::new("shutdown").arg("-c").status();
}
