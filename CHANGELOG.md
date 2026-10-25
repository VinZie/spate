# Changelog

All notable changes to Spate are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [0.2.0] - 2026-10-25

Spate can now update itself. Version 0.1.0 can't, so install this one by hand: quit Spate, then copy the files from the new zip over the old ones (close your browser too if Windows says `spate-cli.exe` is in use). Keeping the same folder keeps browser integration and start at sign-in working; your download list and settings carry over either way.

### Added

- Update prompt. Once a day Spate compares its version with the one on the `main` branch and, once that version's release is published, offers to install it with its release notes. **Update now** downloads the package, checks it against the published SHA-256, swaps the files in place and restarts; downloads that were running continue afterwards. **Help → Check for updates…** looks right away, and **Options → General** turns the daily check off.

### Fixed

- **Help → About → Project page** opened the wrong address.

## [0.1.0] - 2026-10-24

First public release.

### Added

- Multi-connection HTTP and HTTPS downloads (1–32 connections, 8 by default) that write straight into one preallocated sparse file, with no merge step at the end.
- Speed-weighted work splitting, so connections finish together.
- Adaptive connection limits for servers that answer 429 or 503: backs off, honours `Retry-After`, probes upward again and remembers each server's limit.
- Pause and resume with crash-safe checkpoints; `If-Range` and ETag checks restart a download cleanly when the remote file changes.
- HLS (`.m3u8`) downloads with variant selection, AES-128, fMP4 and byte-range segments.
- Desktop app: download list with categories, live segment map, Add URL and Batch dialogs, queue with daily schedule, clipboard watcher, tray icon, light and dark themes.
- `spate-cli`: `get`, `info`, `bench`, `add`, `show`, `status`, Windows-style `/d /p /f /n /a` switches, and the browser's native-messaging host with `install-host` / `uninstall-host`.
- Browser extension for Chrome, Edge, Brave and Firefox (Manifest V3): download capture, context menus, media and HLS detection.
- Portable Windows package with the extension, a quick-start guide and third-party license texts.

### Security

Hardening from a security review before the first release:

- Cookies, `Authorization` and other login headers are sent only to the origin they were collected for. Spate follows redirects itself and re-checks every hop, including for HLS playlists, segments and keys, and never sends them over plain http after https.
- Every finished download carries the Mark of the Web (`Zone.Identifier`), so SmartScreen and Office Protected View apply.
- Capture dialogs no longer take focus; their buttons wait about a second, held keys are ignored, and Enter opens only downloads the user selected.
- The name the user confirms is the name saved; names lose bidirectional-text and invisible characters, and reserved device names are caught.
- Requests made before the user confirms carry no credentials, and clipboard links are not fetched until the user asks.
- Login headers are never written to disk; old download lists are scrubbed.
- The app proves it knows the IPC token before helpers send it anything.
- Partial files carry an ownership token, so stale resume data can't take over another download's file; checksums are verified before the final rename.
- HLS: bounded memory for playlists and segments, and public playlists can't point at local network addresses.
- Browser extension: only the cookies the browser itself would send (SameSite rules), strict-origin-when-cross-origin Referer, no HLS detection or dangerous file names from page-supplied `download` attributes, and media in other sites' frames isn't picked automatically.
- Release workflow: actions pinned to commit SHAs, a read-only build job, and build provenance attestations for public releases.

[0.2.0]: https://github.com/VinZie/spate/releases/tag/v0.2.0
[0.1.0]: https://github.com/VinZie/spate/releases/tag/v0.1.0
