# Contributing to Spate

Thanks for helping. Bug reports, ideas, documentation fixes and code are all welcome.

## Before you start

- **Bugs:** open an issue with the Spate version, your Windows version, the URL or kind of server if you can share it, and what you expected. For a download that fails, `spate-cli info <url>` output helps a lot.
- **Features:** open an issue first for anything larger than a small fix, so we can agree on the approach before you spend time on it.
- **Security problems:** don't open a public issue; follow [`SECURITY.md`](SECURITY.md).

## Building

You need a recent stable Rust toolchain and Node.js 22 or newer.

```powershell
cargo build                                # debug builds of spate.exe and spate-cli.exe
cargo test --workspace                     # all Rust tests; they use a local test server, no internet
cargo clippy --workspace --all-targets
cd extension
npm test                                   # extension unit tests
node scripts/check.mjs                     # manifest and source checks
node build.mjs                             # dist\chromium, dist\firefox and zips
```

To run the app against a throwaway data folder instead of your real one, set `SPATE_DATA_DIR` first:

```powershell
$env:SPATE_DATA_DIR = "$env:TEMP\spate-dev"; cargo run -p spate
```

`pwsh packaging\package-windows.ps1` builds the release zip exactly as the release workflow does.

## Project layout

```
crates/spate-core   the engine and everything shared
  download.rs       one download: probing, connections, segment scheduling, resume
  segments.rs       the segment table and work-splitting rules
  fileio.rs         sparse preallocation and positional writes
  http.rs           HTTP client setup, headers, Retry-After
  hls.rs            HLS (.m3u8) downloads
  manager.rs        the download list, queue, scheduler and persistence
  ipc.rs            token-authenticated local IPC and the native-messaging types
  settings.rs, paths.rs, naming.rs, category.rs, limiter.rs
crates/spate        the desktop app (eframe/egui)
crates/spate-cli    the command line, the native-messaging host and browser registration
extension/          the Manifest V3 browser extension (Chromium and Firefox builds)
packaging/          the Windows package script and the README shipped in the zip
docs/assets/        images used by the README
```

## Guidelines

- **Keep the engine tested.** Changes to `spate-core` should come with a test in `crates/spate-core/tests/` (see `tests/common/mod.rs` for the local test server, which can throttle, cap connections and return errors on demand).
- **No network in tests.** Tests must pass offline and in CI.
- **Timing tests.** A few engine tests compare download speeds. On a busy or slow machine run them one at a time with `cargo test -p spate-core --test engine -- --test-threads=1`, as CI does. When a new test depends on how fast connections start, throttle the test server so the outcome doesn't depend on the machine.
- **Match the surrounding code.** Follow the style, naming and comment density of the file you are editing; run `cargo clippy` and fix new warnings.
- **Mind the security model.** Don't add listeners that web pages can reach, CORS headers, or options that disable the IPC token or origin checks. See the security section of the README.
- **Small pull requests** with a clear description of what changed and why are much easier to review.

## Releasing

1. Update the version in the three `crates/*/Cargo.toml` files, `extension/manifest.json`, `extension/manifest.firefox.json` and `extension/package.json`.
2. Add a section to [`CHANGELOG.md`](CHANGELOG.md).
3. Commit, then tag and push: `git tag v0.2.0 && git push origin v0.2.0`. The release workflow tests, builds the zip and publishes the GitHub release.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE), the same as the rest of the project.
