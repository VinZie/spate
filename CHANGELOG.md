# Changelog

All notable changes to Spate are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

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

[0.1.0]: https://github.com/VinZie/spate/releases/tag/v0.1.0
