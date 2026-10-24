//! `spate-cli`: headless multi-connection downloads, remote control of the
//! Spate app, and the browser extension's native-messaging host.

mod app;
mod bench;
mod fmt;
mod get;
mod info;
mod install;
mod native_host;
mod options;
mod progress;

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use spate_core::http::NetConfig;

pub const EXIT_OK: u8 = 0;
pub const EXIT_ERROR: u8 = 1;
/// Interrupted by Ctrl+C; the download can be resumed.
pub const EXIT_PAUSED: u8 = 130;

pub const CLI_USER_AGENT: &str = concat!("Spate/", env!("CARGO_PKG_VERSION"), " (spate-cli)");

#[derive(Parser)]
#[command(
    name = "spate-cli",
    version,
    about = "Spate command line: fast multi-connection downloads and app control",
    after_help = "Exit codes: 0 success, 1 error, 2 usage error, 130 paused by Ctrl+C.\n\
                  Compatibility switches: spate-cli /d URL [/p dir] [/f name] [/n] [/a]"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download URLs right here in the terminal (resumes automatically)
    Get(get::GetArgs),
    /// Show what a URL would download: name, size, resumability
    Info(info::InfoArgs),
    /// Measure download speed for different connection counts
    Bench(bench::BenchArgs),
    /// Hand URLs to the Spate app (starts it if needed)
    Add(app::AddArgs),
    /// Bring the Spate window to the front (starts it if needed)
    Show,
    /// Show app, data folder and browser-integration status
    Status,
    /// Register the native-messaging host with browsers (current user)
    InstallHost(install::InstallArgs),
    /// Remove the native-messaging host registrations
    UninstallHost,
    /// Run as the browser extension's native-messaging host (stdio)
    NativeHost(native_host::HostArgs),
}

/// Request options shared by commands that talk to servers directly.
#[derive(Args, Clone, Debug, Default)]
pub struct NetArgs {
    /// Extra request header, e.g. -H 'Authorization: Bearer x' (repeatable)
    #[arg(short = 'H', long = "header", value_name = "NAME: VALUE", value_parser = options::parse_header)]
    pub headers: Vec<(String, String)>,
    /// Cookie header value, e.g. --cookie 'session=abc; theme=dark'
    #[arg(long, value_name = "COOKIES")]
    pub cookie: Option<String>,
    /// Referer to send
    #[arg(long, alias = "referrer", value_name = "URL")]
    pub referer: Option<String>,
    /// User-Agent to send instead of the default
    #[arg(long, value_name = "UA")]
    pub user_agent: Option<String>,
    /// Proxy URL (http://, https://, socks5://, socks5h://); default: system proxy
    #[arg(long, value_name = "URL")]
    pub proxy: Option<String>,
}

impl NetArgs {
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut h = self.headers.clone();
        if let Some(c) = self.cookie.as_deref().filter(|c| !c.trim().is_empty()) {
            h.push(("Cookie".into(), c.trim().into()));
        }
        if let Some(r) = self.referer.as_deref().filter(|r| !r.trim().is_empty()) {
            h.push(("Referer".into(), r.trim().into()));
        }
        h
    }

    pub fn net_config(&self, mut base: NetConfig) -> NetConfig {
        // The app's default user agent imitates Chrome, which TLS-fingerprinting
        // CDNs reject from a non-browser client (e.g. Hetzner's speed mirrors
        // drop the connection). Like curl/wget/aria2, identify honestly unless
        // the user configured a user agent.
        if base.user_agent == spate_core::http::DEFAULT_USER_AGENT {
            base.user_agent = CLI_USER_AGENT.into();
        }
        if let Some(ua) = self.user_agent.as_deref().filter(|u| !u.trim().is_empty()) {
            base.user_agent = ua.trim().into();
        }
        if let Some(p) = self.proxy.as_deref().filter(|p| !p.trim().is_empty()) {
            base.proxy = Some(p.trim().into());
        }
        base
    }
}

fn main() -> ExitCode {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();

    // Browsers start the host with their own argument conventions; detect
    // those before clap, which would print usage errors to stdout/stderr.
    if let Some(launch) = native_host::detect(&argv) {
        return ExitCode::from(native_host::run(launch));
    }

    let (argv, warnings) = options::translate_switches(argv);
    for w in warnings {
        eprintln!("warning: {w}");
    }
    let cli = Cli::parse_from(argv);
    let code = match cli.command {
        Command::Get(a) => block_on(get::run(a)),
        Command::Info(a) => block_on(info::run(a)),
        Command::Bench(a) => block_on(bench::run(a)),
        Command::Add(a) => app::run_add(a),
        Command::Show => app::run_show(),
        Command::Status => install::run_status(),
        Command::InstallHost(a) => install::run_install(a),
        Command::UninstallHost => install::run_uninstall(),
        Command::NativeHost(a) => native_host::run(native_host::Launch::Explicit { origin: a.origin }),
    };
    ExitCode::from(code)
}

fn block_on<F: std::future::Future<Output = u8>>(f: F) -> u8 {
    match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt.block_on(f),
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            EXIT_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn get_flags_parse() {
        let cli = Cli::try_parse_from([
            "spate-cli", "get", "https://x/a.bin", "-c", "16", "--limit", "2M", "-H", "X-A: 1", "--cookie", "a=b",
            "--min-split", "512K", "-o", "b.bin", "-d", "out",
        ])
        .unwrap();
        let Command::Get(g) = cli.command else { panic!("expected get") };
        assert_eq!(g.connections, Some(16));
        assert_eq!(g.limit, Some(2 << 20));
        assert_eq!(g.min_split, Some(512 << 10));
        assert_eq!(g.output.as_deref(), Some("b.bin"));
        let h = g.net.headers();
        assert!(h.contains(&("X-A".into(), "1".into())));
        assert!(h.contains(&("Cookie".into(), "a=b".into())));
        let net = g.net.net_config(NetConfig::default());
        assert_eq!(net.user_agent, CLI_USER_AGENT);
        let custom = NetConfig { user_agent: "Mine/1".into(), ..NetConfig::default() };
        assert_eq!(g.net.net_config(custom).user_agent, "Mine/1", "a configured UA is kept");
        assert!(Cli::try_parse_from(["spate-cli", "get", "https://x", "-c", "0"]).is_err());
        assert!(Cli::try_parse_from(["spate-cli", "get", "https://x", "--limit", "fast"]).is_err());
    }
}
