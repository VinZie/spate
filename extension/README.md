# Spate browser extension

Connects Chromium-based browsers (Chrome, Edge, Brave, Vivaldi, Opera) and
Firefox to the Spate download manager:

- **Download capture**: downloads that match your file types are paused, handed to
  Spate together with the cookies, referrer and user agent they need, and then
  removed from the browser. If Spate cannot take them, the browser download
  resumes and the toolbar badge turns red (`!`) with the reason.
- **Context menus**: *Download with Spate* (links, images, video, audio,
  frames), *Download selected links with Spate*, *Download all links with Spate*
  (unique http(s) links and media sources, sent as one batch with the Spate
  confirmation dialog), and *Download this video/stream with Spate* when the
  page has loaded media (one entry per stream when there are several). On
  Chromium, links on `chrome://downloads` get *Download again with Spate*.
- **Media detection**: responses that are video, audio, HLS playlists
  (`.m3u8`) or DASH manifests (`.mpd`) are listed per tab. The badge shows the
  count; the popup lists type, size and address with a *Download* button.
  Fragments under 500 KB and segment files (`.ts`, `.m4s`, ...) are ignored. HLS
  is sent with `kind: "hls"`; DASH is listed but marked unsupported.
- **Popup**: capture on/off switch, connection status (`Connected v…`, or how to
  fix it), detected media, *Download all links*, link to the options.
- **Options**: file types to capture (wildcards allowed, e.g. `r??`), minimum
  size, excluded sites (wildcards and path prefixes), show the Spate
  confirmation dialog, capture all downloads, media detection on/off.

There is no remote code, no analytics and no network access besides the
browser's own requests: the extension only talks to the local native
messaging host.

## Install

### 1. Register the native messaging host

The extension reaches Spate through the native messaging host
`com.spate.host`, implemented by `spate-cli`. Register it once (per user):

```
spate-cli install-host
```

This writes the host manifest for the supported browsers and allows the
Chromium extension ID `lliejfalegflnfhnefcinabnpbpbbble` and the Firefox
add-on ID `spate@vinzie.github.io`. The Spate app must be running for downloads to
be accepted; the popup tells you when it is not.

### 2a. Chrome, Edge, Brave and other Chromium browsers (110+)

1. Open `chrome://extensions` (Edge: `edge://extensions`).
2. Turn on **Developer mode**.
3. Click **Load unpacked** and select this `extension/` folder (or
   `dist/chromium/` after a build).

The `key` in `manifest.json` pins the extension ID to
`lliejfalegflnfhnefcinabnpbpbbble`, the ID the native host allows. Do not remove
it, or the browser will refuse the native connection ("Access to the specified
native messaging host is forbidden").

### 2b. Firefox (140+)

1. Build the Firefox package: `node build.mjs` (in this folder; needs Node 18+).
2. Open `about:debugging#/runtime/this-firefox`, click **Load Temporary Add-on…**
   and pick `dist/firefox/manifest.json` (or `dist/spate-firefox-<version>.zip`).
3. Firefox treats MV3 site access as optional: if the popup shows *Spate needs
   access to websites*, click **Allow** (or enable it under about:addons →
   Spate → Permissions). Without it cookies cannot be read and media cannot be
   detected.

Temporary add-ons are removed when Firefox restarts. For a permanent install,
sign the zip through addons.mozilla.org (it can be an unlisted, self-distributed
add-on), or use Firefox Developer Edition / Nightly with
`xpinstall.signatures.required` set to `false`.

## Permissions

| Permission | Why |
| --- | --- |
| `downloads` | See new browser downloads, pause them, then cancel and erase the ones Spate accepted (or resume them if it did not). |
| `nativeMessaging` | Send requests to the local `com.spate.host` program. |
| `contextMenus` | The *Download with Spate* menu entries. |
| `cookies` | Read the cookies of the download URL, so Spate can fetch files behind a login. They are only passed to the local Spate. |
| `storage` | Keep settings (`storage.local`) and the per-tab media list across service-worker restarts (`storage.session`). |
| `webRequest` | Observe response headers (Content-Type/Length) to detect video, audio and streams, and notice form submissions (POST) whose downloads Spate could not repeat. Read-only; nothing is blocked or modified. |
| `webNavigation` | Clear a tab's media list when it navigates to a new page. |
| `scripting` | Collect the links of a page (or selection) when you choose *Download all/selected links*. Only runs on that request. |
| host access `http://*/*`, `https://*/*` | Needed by `cookies` and `webRequest` for arbitrary download sites, and by `scripting` for link collection. |

Not requested: `tabs` (tab URL/title come with host access), `notifications`,
`downloads.shelf`/`downloads.ui`, `proxy`, `management`, `webRequestBlocking`.

## How it talks to Spate

One native message per action (`runtime.sendNativeMessage`):

```json
{"type": "ping"}
{"type": "download", "interactive": true,
 "items": [{"url": "...", "fileName": "...", "referrer": "...", "cookies": "a=1; b=2",
            "userAgent": "...", "fileSize": 12345, "mime": "application/zip", "kind": "http"}]}
```

`kind` is `"http"` or `"hls"`; fields the browser does not know are omitted.
The host replies `{"ok": true|false, "error"?: "...", "version": "..."}` and
forwards the request to the desktop app over its token-authenticated loopback
IPC (see `crates/spate-core/src/ipc.rs`).

Capture rules, in order: capture switch on → http(s) URL (no `blob:`/`data:`) →
not started by another extension → not the response to a form POST → not on an excluded site (download URL or
referrer) → not below the minimum size (when the size is known) → file type
listed (from the file name, else the URL, else the MIME type) unless *capture
all downloads* is on. On Chromium the decision is made in
`downloads.onDeterminingFilename`, which carries the final file name and holds
the download (so no *Save as* dialog appears first); Firefox uses
`downloads.onCreated`.

## Development

```
node build.mjs              # dist/chromium, dist/firefox and both zips
node --test test/*.test.mjs # unit and integration tests (no dependencies)
node scripts/check.mjs      # node --check on every JS file, JSON validity
node scripts/make-icons.mjs # regenerate icons/*.png
```

Layout:

- `manifest.json`: Chromium MV3 manifest (module service worker); the folder
  loads unpacked as is.
- `manifest.firefox.json`: Firefox MV3 manifest; `build.mjs` copies the version
  from `manifest.json`.
- `src/background.js`: capture, menus, media detection, message API.
- `src/lib/core.js`: pure helpers (wildcards, file-type filter, capture
  decision, media classification, cookie header, formatting), unit-tested.
- `src/lib/compat.js`: `browser.*` promises vs `chrome.*` callbacks.
- `src/lib/native.js`, `settings.js`, `media-store.js`, `page-scripts.js`.
- `src/popup.*`, `src/options.*`, `src/ui.css`: the two pages.
- `build.mjs`: Firefox runs MV3 backgrounds as event pages from
  `background.scripts`, so the module graph is bundled into one classic script
  (`src/background.bundle.js`); also writes the zips.
- `test/`: `core.test.mjs` (helpers), `background.test.mjs` (the Chromium
  modules and the Firefox bundle against a mocked browser API),
  `build.test.mjs` (manifests, icons, build output, zip format).

## Known limitations

- No in-page "download this video" button; media are offered through the
  badge, popup and context menu.
- DASH (`.mpd`) streams and sites that assemble media in the page (Media Source
  Extensions with no playlist URL) cannot be downloaded; there are no
  site-specific extractors.
- Only cookies, referrer and user agent are handed over. Downloads produced by
  a form submission (POST) are detected and left to the browser; downloads
  that need other request headers (e.g. `Authorization`) may fail in Spate.
- The browser's proxy settings are not passed to Spate.
- There is no keyboard modifier to bypass capture for a single download; use the
  popup switch or an excluded site.
