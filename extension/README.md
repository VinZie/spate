# Spate browser extension

Connects Chromium-based browsers (Chrome, Edge, Brave, Vivaldi, Opera) and
Firefox to the Spate download manager:

- **Download capture**: downloads that match your file types are paused, handed to
  Spate together with the cookies, referrer and user agent they need (only
  what the browser itself would send, see
  [Cookies and referrer](#cookies-and-referrer)), and then removed from the
  browser. If Spate cannot take them, the browser download resumes and the
  toolbar badge turns red (`!`) with the reason.
- **Context menus**: *Download with Spate* (links, images, video, audio,
  frames), *Download selected links with Spate*, *Download all links with Spate*
  (unique http(s) links and media sources, sent as one batch with the Spate
  confirmation dialog), and *Download this video/stream with Spate* when the
  page has loaded media (one entry per stream when there are several). On
  Chromium, links on `chrome://downloads` get *Download again with Spate*.
  A link's `download="name"` attribute is used only for links to the page's
  own origin, as browsers do, and never for shortcut-type names (`.lnk`,
  `.url`, `.scf`, `.library-ms`, ...); Spate then names the file from the
  server or the URL.
- **Media detection**: responses that are video, audio, HLS playlists
  (`.m3u8`) or DASH manifests (`.mpd`) are listed per tab. The badge shows the
  count; the popup lists type, size and address with a *Download* button.
  Fragments under 500 KB and segment files (`.ts`, `.m4s`, ...) are ignored. HLS
  (recognized by its URL path or Content-Type, never by a name the page
  suggests) is sent with `kind: "hls"`; DASH is listed but marked unsupported.
  *Download this video/stream* picks from what the page itself loaded (the
  first HLS playlist, else the largest video, else the largest audio), or from
  frames of the same origin when the page loaded nothing; media from frames of
  other sites (ads, say) is never picked automatically and is offered by name
  in the menu and the popup instead.
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

1. Take `spate-firefox-<version>.zip` from the Spate release, or build it with
   `node build.mjs` in this folder (needs Node 18+; it lands in `dist/`).
2. Open `about:debugging#/runtime/this-firefox`, click **Load Temporary Add-on…**
   and pick the zip (or `dist/firefox/manifest.json` after a build).
3. Firefox treats MV3 site access as optional: if the popup shows *Spate needs
   access to websites*, click **Allow** (or enable it under about:addons →
   Spate → Permissions). Without it cookies cannot be read and media cannot be
   detected.

Temporary add-ons are removed when Firefox restarts; load it again after a
restart.

**Signed builds and the add-on ID.** The native host trusts exactly one
Firefox add-on ID, `spate@vinzie.github.io`: whatever add-on runs under that
ID can send download requests to Spate. addons.mozilla.org gives an ID to
whoever registers it first, so only the Spate maintainer publishes the signed
build, under this ID, and has to claim the ID there before anyone else does.
Until that signed build is available, use the temporary
add-on from the release zip as above. Do not submit or sign the extension on
addons.mozilla.org yourself, and do not install a "Spate" add-on from anyone
else. (For development, Firefox Developer Edition and Nightly can run the
unsigned build permanently with `xpinstall.signatures.required` set to
`false`.)

## Permissions

| Permission | Why |
| --- | --- |
| `downloads` | See new browser downloads, pause them, then cancel and erase the ones Spate accepted (or resume them if it did not). |
| `nativeMessaging` | Send requests to the local `com.spate.host` program. |
| `contextMenus` | The *Download with Spate* menu entries. |
| `cookies` | Read the cookies of the download URL, so Spate can fetch files behind a login. Only those the browser would send for that download are kept (see below), and they are only passed to the local Spate. |
| `storage` | Keep settings (`storage.local`) and the per-tab media list across service-worker restarts (`storage.session`). |
| `webRequest` | Observe response headers (Content-Type/Length) to detect video, audio and streams (and which frame loaded them), notice form submissions (POST) whose downloads Spate could not repeat, and see which page started a download (for the cookie rules). Read-only; nothing is blocked or modified. |
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

## Cookies and referrer

An item carries what the browser itself would send if the page the download
comes from opened its address as a top-level navigation (a download is one).
For a download the browser started, that page is the one that started the
request, as webRequest reports it, even if the page suppressed its Referer.

- Cookies come from the tab's cookie store (private windows and Firefox
  containers included). Partitioned (CHIPS) cookies only go when their
  partition's top-level site is that page's site.
- When the page is on another site than the download (another registrable
  domain or scheme, or an opaque origin such as a sandboxed frame),
  `SameSite=Strict` cookies stay behind and `SameSite=None` cookies only go
  over https; `Lax` and unspecified ones go along, as for any link you follow.
  An address you typed or pasted, and links on Chromium's downloads page, count
  as the same site. A browser download whose start was not seen and that has
  no Referer counts as another site.
- There is no public suffix list in the extension, so "same site" is only
  assumed for the same host, or for a domain cookie whose domain covers both
  hosts. A host-only `SameSite=Strict` cookie therefore stays behind when a
  sibling subdomain (`www.example.com` linking to `dl.example.com`) starts the
  download, where the browser would send it.
- The referrer follows `strict-origin-when-cross-origin`, the browsers'
  default: the full page address (without `#fragment`) to the same origin,
  only the origin (`https://example.com/`) to other origins, and none from
  https to http.

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
  decision, media classification, cookie selection and header, referrer,
  file names, formatting), unit-tested.
- `src/lib/compat.js`: `browser.*` promises vs `chrome.*` callbacks.
- `src/lib/native.js`, `settings.js`, `media-store.js`, `page-scripts.js`.
- `src/popup.*`, `src/options.*`, `src/ui.css`: the two pages.
- `build.mjs`: Firefox runs MV3 backgrounds as event pages from
  `background.scripts`, so the module graph is bundled into one classic script
  (`src/background.bundle.js`); also writes the zips.
- `test/`: `core.test.mjs` (helpers), `background.test.mjs` (the Chromium
  modules and the Firefox bundle against a mocked browser API),
  `page-scripts.test.mjs` (the injected link collector, serialized and run
  against a fake DOM), `build.test.mjs` (manifests, icons, build output, zip
  format).

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
