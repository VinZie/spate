// Pure helpers shared by the background worker, the popup, the options page
// and the Node tests. Nothing in this module touches browser APIs, so it can
// be imported anywhere (and bundled into the Firefox background script).

export const NATIVE_HOST = 'com.spate.host';

/** File types captured by default (lower case, no dot; `*` and `?` allowed). */
export const DEFAULT_EXTENSIONS = [
  // archives and disk images
  '7z', 'zip', 'rar', 'r??', 'gz', 'tgz', 'bz2', 'xz', 'zst', 'tar', 'cab', 'lz', 'lzh', 'arj',
  'iso', 'img', 'dmg', 'vhd', 'vhdx', 'qcow2',
  // installers and packages
  'exe', 'msi', 'msix', 'msixbundle', 'appx', 'appxbundle', 'apk', 'xapk', 'deb', 'rpm', 'pkg',
  'appimage', 'flatpak', 'snap', 'bin', 'run', 'jar',
  // documents
  'pdf', 'epub', 'djvu', 'ps',
  // video
  'mp4', 'm4v', 'mkv', 'webm', 'avi', 'mov', 'wmv', 'flv', 'mpg', 'mpeg', 'm2ts', '3gp', 'ogv', 'rm', 'rmvb',
  // audio
  'mp3', 'm4a', 'aac', 'flac', 'wav', 'ogg', 'oga', 'opus', 'wma', 'aif', 'aiff', 'ape',
  // other large payloads
  'torrent', 'unitypackage', 'nupkg', 'whl',
];

export const DEFAULT_SETTINGS = Object.freeze({
  enabled: true,
  captureAll: false,
  extensions: Object.freeze([...DEFAULT_EXTENSIONS]),
  minSizeKB: 0,
  excludedSites: Object.freeze([]),
  interactive: true,
  detectMedia: true,
});

/** Responses smaller than this are treated as fragments, not media files. */
export const MIN_MEDIA_BYTES = 500 * 1024;

const MAX_MIN_SIZE_KB = 100 * 1024 * 1024; // 100 GB, just a sanity cap

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/** Split a free-form list (spaces, commas, semicolons, new lines). */
export function splitList(input) {
  if (Array.isArray(input)) return input.flatMap(splitList);
  if (typeof input !== 'string') return [];
  return input.split(/[\s,;]+/).map((s) => s.trim()).filter(Boolean);
}

/** Normalize a file type list: lower case, no leading `*.` or `.`, unique. */
export function parseExtensionList(input) {
  const out = [];
  for (let raw of splitList(input)) {
    raw = raw.toLowerCase().replace(/^\*?\./, '');
    if (!/^[a-z0-9*?_+\-]{1,16}$/.test(raw)) continue;
    if (!out.includes(raw)) out.push(raw);
  }
  return out;
}

/** Normalize excluded-site patterns: one per entry, lower case, unique. */
export function parseSiteList(input) {
  const out = [];
  for (let raw of splitList(input)) {
    raw = raw.toLowerCase();
    if (raw.length > 512) continue;
    if (!out.includes(raw)) out.push(raw);
  }
  return out;
}

export function normalizeSettings(raw) {
  const src = raw && typeof raw === 'object' ? raw : {};
  const bool = (key) => (typeof src[key] === 'boolean' ? src[key] : DEFAULT_SETTINGS[key]);
  let minSizeKB = Number(src.minSizeKB);
  if (!Number.isFinite(minSizeKB) || minSizeKB < 0) minSizeKB = DEFAULT_SETTINGS.minSizeKB;
  minSizeKB = Math.min(Math.floor(minSizeKB), MAX_MIN_SIZE_KB);
  return {
    enabled: bool('enabled'),
    captureAll: bool('captureAll'),
    extensions: src.extensions === undefined ? [...DEFAULT_SETTINGS.extensions] : parseExtensionList(src.extensions),
    minSizeKB,
    excludedSites: src.excludedSites === undefined ? [] : parseSiteList(src.excludedSites),
    interactive: bool('interactive'),
    detectMedia: bool('detectMedia'),
  };
}

// ---------------------------------------------------------------------------
// Wildcards, sites and file types
// ---------------------------------------------------------------------------

/** `*` matches any run of characters, `?` exactly one; case-insensitive. */
export function globToRegExp(glob) {
  let re = '';
  for (const ch of String(glob)) {
    if (ch === '*') re += '.*';
    else if (ch === '?') re += '.';
    else re += ch.replace(/[\\^$.+()[\]{}|/]/g, '\\$&');
  }
  return new RegExp(`^${re}$`, 'i');
}

export function hasWildcard(s) {
  return /[*?]/.test(s);
}

/** Parsed URL when `url` is an absolute http(s) URL, otherwise null. */
export function parseHttpUrl(url) {
  try {
    const u = new URL(url);
    return u.protocol === 'http:' || u.protocol === 'https:' ? u : null;
  } catch {
    return null;
  }
}

/** Lower-case host name of an http(s) URL ('' otherwise). */
export function hostOf(url) {
  const u = parseHttpUrl(url);
  return u ? u.hostname.toLowerCase() : '';
}

/**
 * Whether `url` matches one excluded-site pattern.
 *
 * - `example.com` matches that host and all of its subdomains.
 * - `*.example.com` matches the same (bare domain included).
 * - Other wildcards (`cdn?.example.*`) are matched against the host name.
 * - A pattern containing `/` is a prefix match against `host/path?query`
 *   (wildcards allowed), e.g. `example.com/downloads/`.
 * A leading scheme (`https://`, `*://`) in the pattern is ignored.
 */
export function siteMatches(pattern, url) {
  const u = parseHttpUrl(url);
  if (!u) return false;
  let p = String(pattern || '').trim().toLowerCase().replace(/^[a-z*]+:\/\//, '');
  if (!p) return false;
  const host = u.hostname.toLowerCase().replace(/\.$/, '');
  const slash = p.indexOf('/');
  if (slash >= 0) {
    const hostPart = p.slice(0, slash).replace(/:\d+$/, '');
    const pathPart = p.slice(slash);
    if (!hostMatches(hostPart, host)) return false;
    const target = `${u.pathname}${u.search}`;
    const glob = pathPart.endsWith('*') ? pathPart : `${pathPart}*`;
    return globToRegExp(glob).test(target);
  }
  return hostMatches(p.replace(/:\d+$/, ''), host);
}

function hostMatches(pattern, host) {
  if (!pattern) return false;
  if (pattern.startsWith('*.') && !hasWildcard(pattern.slice(2))) {
    const base = pattern.slice(2);
    return host === base || host.endsWith(`.${base}`);
  }
  if (hasWildcard(pattern)) return globToRegExp(pattern).test(host);
  return host === pattern || host.endsWith(`.${pattern}`);
}

/** Whether any of `urls` matches any of `patterns`. */
export function isExcluded(patterns, ...urls) {
  if (!Array.isArray(patterns) || !patterns.length) return false;
  return urls.some((url) => url && patterns.some((p) => siteMatches(p, url)));
}

/** Last path component of a file system path (either separator). */
export function basename(path) {
  if (!path) return '';
  const parts = String(path).split(/[\\/]/);
  return parts[parts.length - 1] || '';
}

/** Lower-case extension of a file name or URL path, without the dot. */
export function fileExtension(nameOrUrl) {
  if (!nameOrUrl) return '';
  let name = String(nameOrUrl);
  const u = parseHttpUrl(name);
  if (u) name = u.pathname;
  else name = name.replace(/[?#].*$/, '');
  name = basename(name);
  const dot = name.lastIndexOf('.');
  if (dot <= 0 || dot === name.length - 1) return '';
  const ext = name.slice(dot + 1).toLowerCase();
  return /^[a-z0-9_+\-]{1,16}$/.test(ext) ? ext : '';
}

/** Whether `ext` is listed (patterns may use `*` / `?`). */
export function extensionMatches(ext, list) {
  if (!ext || !Array.isArray(list)) return false;
  const e = ext.toLowerCase();
  return list.some((p) => (hasWildcard(p) ? globToRegExp(p).test(e) : p === e));
}

const MIME_EXTENSIONS = {
  'application/zip': 'zip',
  'application/x-zip-compressed': 'zip',
  'application/x-rar-compressed': 'rar',
  'application/vnd.rar': 'rar',
  'application/x-7z-compressed': '7z',
  'application/gzip': 'gz',
  'application/x-gzip': 'gz',
  'application/x-bzip2': 'bz2',
  'application/x-xz': 'xz',
  'application/x-tar': 'tar',
  'application/zstd': 'zst',
  'application/x-iso9660-image': 'iso',
  'application/x-apple-diskimage': 'dmg',
  'application/x-msdownload': 'exe',
  'application/x-msdos-program': 'exe',
  'application/vnd.microsoft.portable-executable': 'exe',
  'application/x-msi': 'msi',
  'application/x-ms-installer': 'msi',
  'application/vnd.android.package-archive': 'apk',
  'application/vnd.debian.binary-package': 'deb',
  'application/x-rpm': 'rpm',
  'application/java-archive': 'jar',
  'application/pdf': 'pdf',
  'application/epub+zip': 'epub',
  'application/x-bittorrent': 'torrent',
  'video/mp4': 'mp4',
  'video/x-matroska': 'mkv',
  'video/webm': 'webm',
  'video/quicktime': 'mov',
  'video/x-msvideo': 'avi',
  'video/x-ms-wmv': 'wmv',
  'video/x-flv': 'flv',
  'video/mpeg': 'mpg',
  'video/mp2t': 'ts',
  'video/3gpp': '3gp',
  'video/ogg': 'ogv',
  'audio/mpeg': 'mp3',
  'audio/mp4': 'm4a',
  'audio/x-m4a': 'm4a',
  'audio/aac': 'aac',
  'audio/flac': 'flac',
  'audio/x-flac': 'flac',
  'audio/wav': 'wav',
  'audio/x-wav': 'wav',
  'audio/ogg': 'ogg',
  'audio/opus': 'opus',
  'audio/x-ms-wma': 'wma',
};

/** Best-effort extension for a MIME type ('' when unknown or generic). */
export function mimeExtension(mime) {
  return MIME_EXTENSIONS[baseMime(mime)] || '';
}

export function baseMime(mime) {
  return String(mime || '').split(';')[0].trim().toLowerCase();
}

// ---------------------------------------------------------------------------
// Download capture decision
// ---------------------------------------------------------------------------

/**
 * Decide whether a browser download should be handed to Spate.
 * `dl`: {url, referrer, fileName, mime, size, byExtensionId, state, method}
 * Returns {capture, reason}.
 */
export function shouldCapture(dl, settings) {
  const s = settings || DEFAULT_SETTINGS;
  if (!s.enabled) return { capture: false, reason: 'disabled' };
  if (!parseHttpUrl(dl.url)) return { capture: false, reason: 'unsupported-scheme' };
  if (dl.byExtensionId) return { capture: false, reason: 'started-by-extension' };
  // Spate re-requests with GET; a form submission's response must stay in the browser.
  if (dl.method && dl.method.toUpperCase() !== 'GET') return { capture: false, reason: 'not-a-get-request' };
  if (dl.state && dl.state !== 'in_progress') return { capture: false, reason: 'not-in-progress' };
  if (isExcluded(s.excludedSites, dl.url, dl.referrer)) return { capture: false, reason: 'excluded-site' };
  const size = positiveInt(dl.size);
  if (size && s.minSizeKB > 0 && size < s.minSizeKB * 1024) return { capture: false, reason: 'too-small' };
  if (!s.captureAll) {
    const ext = fileExtension(dl.fileName) || fileExtension(dl.url) || mimeExtension(dl.mime);
    if (!extensionMatches(ext, s.extensions)) return { capture: false, reason: 'type-not-listed' };
  }
  return { capture: true, reason: 'ok' };
}

export function positiveInt(n) {
  const v = Number(n);
  return Number.isFinite(v) && v > 0 ? Math.floor(v) : undefined;
}

// ---------------------------------------------------------------------------
// Media detection
// ---------------------------------------------------------------------------

const HLS_MIMES = new Set([
  'application/vnd.apple.mpegurl',
  'application/x-mpegurl',
  'audio/mpegurl',
  'audio/x-mpegurl',
  'video/x-mpegurl',
]);
const DASH_MIMES = new Set(['application/dash+xml', 'video/vnd.mpeg.dash.mpd']);
const SEGMENT_MIMES = new Set(['video/mp2t', 'video/iso.segment', 'audio/iso.segment']);
const SEGMENT_EXTS = new Set(['ts', 'm4s', 'm4f', 'cmfv', 'cmfa', 'cmft', 'fmp4', 'm4i']);
const VIDEO_EXTS = new Set(['mp4', 'm4v', 'webm', 'mkv', 'mov', 'flv', 'avi', 'wmv', 'mpg', 'mpeg', 'ogv', '3gp']);
const AUDIO_EXTS = new Set(['mp3', 'm4a', 'aac', 'ogg', 'oga', 'opus', 'wav', 'flac', 'wma']);
const GENERIC_MIMES = new Set(['', 'application/octet-stream', 'binary/octet-stream', 'application/binary', 'application/x-binary', 'application/force-download', 'application/unknown']);
/** Content types that can never be media even if the URL says otherwise. */
const NON_MEDIA_MIME = /^(text\/html|text\/css|application\/(json|javascript|xhtml\+xml)|image\/)/;
/** Query parameters that only select a byte range of the same resource. */
const RANGE_PARAMS = ['range', 'rn', 'rbuf', 'bytestart', 'byteend'];

/** Header list (webRequest format) → lower-case name → value map. */
export function headerMap(headers) {
  const map = {};
  for (const h of headers || []) {
    if (h && h.name) map[h.name.toLowerCase()] = h.value ?? '';
  }
  return map;
}

/** Full resource size from Content-Range (206) or Content-Length. */
export function responseSize(headers) {
  const range = /\/\s*(\d+)\s*$/.exec(headers['content-range'] || '');
  if (range) return positiveInt(range[1]);
  return positiveInt(headers['content-length']);
}

/**
 * Classify a response as downloadable media.
 * Returns null or {kind: 'hls'|'dash'|'video'|'audio', mime, ext, size}.
 */
export function classifyMedia({ url, contentType, size, statusCode }) {
  const u = parseHttpUrl(url);
  if (!u) return null;
  if (statusCode && (statusCode < 200 || statusCode >= 300)) return null;
  const mime = baseMime(contentType);
  const ext = fileExtension(u.pathname);
  const bytes = positiveInt(size);
  const result = (kind) => ({ kind, mime: mime || null, ext: ext || null, size: bytes ?? null });

  if (HLS_MIMES.has(mime)) return result('hls');
  if (DASH_MIMES.has(mime)) return result('dash');
  if (NON_MEDIA_MIME.test(mime)) return null;
  if (ext === 'm3u8') return result('hls');
  if (ext === 'mpd') return result('dash');
  if (SEGMENT_MIMES.has(mime) || SEGMENT_EXTS.has(ext)) return null;

  let kind = null;
  if (mime.startsWith('video/')) kind = 'video';
  else if (mime.startsWith('audio/')) kind = 'audio';
  else if (GENERIC_MIMES.has(mime)) {
    if (VIDEO_EXTS.has(ext)) kind = 'video';
    else if (AUDIO_EXTS.has(ext)) kind = 'audio';
  }
  if (!kind) return null;
  if (bytes !== undefined && bytes < MIN_MEDIA_BYTES) return null;
  return result(kind);
}

/** Key used to de-duplicate detected media (drops hash and range params). */
export function mediaKey(url) {
  try {
    const u = new URL(url);
    u.hash = '';
    for (const p of RANGE_PARAMS) u.searchParams.delete(p);
    return u.href;
  } catch {
    return String(url);
  }
}

/** Label shown for a detected media entry, e.g. "HLS", "MP4", "DASH". */
export function mediaLabel(media) {
  if (!media) return '';
  if (media.kind === 'hls') return 'HLS';
  if (media.kind === 'dash') return 'DASH';
  if (media.ext) return media.ext.toUpperCase();
  const fromMime = mimeExtension(media.mime);
  if (fromMime) return fromMime.toUpperCase();
  return media.kind === 'audio' ? 'Audio' : 'Video';
}

export function mediaSupported(media) {
  return Boolean(media) && media.kind !== 'dash';
}

/**
 * Pick the entry to download when the user asks for "this video":
 * the first HLS playlist detected (usually the master playlist), otherwise the
 * largest video, otherwise the largest audio. DASH is never picked.
 */
export function pickBestMedia(list) {
  const usable = (list || []).filter(mediaSupported);
  const hls = usable.filter((m) => m.kind === 'hls').sort((a, b) => (a.detectedAt ?? 0) - (b.detectedAt ?? 0));
  if (hls.length) return hls[0];
  const bySize = (a, b) => (b.size ?? 0) - (a.size ?? 0);
  const video = usable.filter((m) => m.kind === 'video').sort(bySize);
  if (video.length) return video[0];
  const audio = usable.filter((m) => m.kind === 'audio').sort(bySize);
  return audio[0] || null;
}

/** "hls" for HLS playlists, otherwise "http". */
export function downloadKind(url, mime, fileName) {
  if (HLS_MIMES.has(baseMime(mime))) return 'hls';
  if (fileExtension(fileName) === 'm3u8' || fileExtension(url) === 'm3u8') return 'hls';
  return 'http';
}

// ---------------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------------

/** Registrable-ish host of a partition key's top-level site ("https://a.b" → "a.b"). */
function siteHost(site) {
  try {
    return new URL(site).hostname.toLowerCase();
  } catch {
    return String(site || '').replace(/^[a-z]+:\/\//i, '').replace(/[:/].*$/, '').toLowerCase();
  }
}

/**
 * Keep the cookies a browser would send for a request made from a page on
 * `topLevelHost`: all unpartitioned cookies, plus partitioned (CHIPS)
 * cookies whose partition's top-level site is that host or a parent of it.
 */
export function selectCookies(cookies, topLevelHost) {
  const host = String(topLevelHost || '').toLowerCase();
  return (cookies || []).filter((c) => {
    const site = c && c.partitionKey && c.partitionKey.topLevelSite;
    if (!site) return true;
    if (!host) return false;
    const h = siteHost(site);
    return host === h || host.endsWith(`.${h}`);
  });
}

/** Build a `Cookie` header value (longest path first, duplicates removed). */
export function buildCookieHeader(cookies) {
  const seen = new Set();
  const list = (cookies || [])
    .filter((c) => c && typeof c.name === 'string' && typeof c.value === 'string' && (c.name || c.value))
    .map((c, i) => ({ c, i }))
    .sort((a, b) => (b.c.path || '/').length - (a.c.path || '/').length || a.i - b.i);
  const parts = [];
  for (const { c } of list) {
    const id = `${c.name}\u0000${c.domain || ''}\u0000${c.path || ''}\u0000${(c.partitionKey && c.partitionKey.topLevelSite) || ''}`;
    if (seen.has(id)) continue;
    seen.add(id);
    parts.push(c.name ? `${c.name}=${c.value}` : c.value);
  }
  return parts.join('; ');
}

// ---------------------------------------------------------------------------
// Download items and messages
// ---------------------------------------------------------------------------

/** Drop null / undefined / empty-string fields; keep sizes only if positive. */
export function cleanItem(item) {
  const out = {};
  for (const [k, v] of Object.entries(item || {})) {
    if (v === undefined || v === null || v === '') continue;
    if (k === 'fileSize') {
      const n = positiveInt(v);
      if (n !== undefined) out[k] = n;
      continue;
    }
    out[k] = v;
  }
  return out;
}

/** Remove characters that are invalid in Windows/macOS/Linux file names. */
export function sanitizeFileName(name, maxLength = 180) {
  let s = String(name || '')
    .replace(/[\\/:*?"<>|\u0000-\u001f\u007f]+/g, '_')
    .replace(/\s+/g, ' ')
    .trim()
    .replace(/^[.\s]+|[.\s]+$/g, '');
  if (s.length > maxLength) {
    const ext = fileExtension(s);
    const keep = ext ? maxLength - ext.length - 1 : maxLength;
    s = ext ? `${s.slice(0, keep).trimEnd()}.${ext}` : s.slice(0, keep).trimEnd();
  }
  return s;
}

/** Decoded last path segment of a URL ('' when there is none). */
export function fileNameFromUrl(url) {
  const u = parseHttpUrl(url);
  if (!u) return '';
  let seg = basename(u.pathname);
  try {
    seg = decodeURIComponent(seg);
  } catch {
    /* keep raw */
  }
  return sanitizeFileName(seg);
}

/**
 * File name for a detected media entry: the URL's own name for direct
 * files, the page title for streams (Spate saves HLS as one .ts file).
 */
export function mediaFileName(media, pageTitle) {
  const title = sanitizeFileName(pageTitle || '', 120);
  if (media.kind === 'hls') return title ? `${title}.ts` : '';
  const fromUrl = fileNameFromUrl(media.url);
  if (fileExtension(fromUrl)) return fromUrl;
  const ext = media.ext || mimeExtension(media.mime) || (media.kind === 'audio' ? 'mp3' : 'mp4');
  const stem = title || fromUrl || 'media';
  return `${stem}.${ext}`;
}

/** Classify a native-messaging failure for the UI. */
export function nativeErrorKind(message) {
  const m = String(message || '');
  if (/host not found|no such native application|not registered|native messaging host.*(missing|not installed)|manifest/i.test(m)) {
    return 'not-installed';
  }
  if (/forbidden|not allowed|access.*denied/i.test(m)) return 'forbidden';
  if (/not running|connection refused|actively refused|ipc\.json|cannot connect|could not connect/i.test(m)) {
    return 'app-not-running';
  }
  if (/timed? ?out/i.test(m)) return 'timeout';
  if (/exited|disconnected|communicating/i.test(m)) return 'host-error';
  return 'other';
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

export function formatSize(bytes) {
  const n = positiveInt(bytes);
  if (n === undefined) return '';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${v} B` : `${v.toFixed(v >= 100 ? 0 : 1)} ${units[i]}`;
}

/** "host/…/last-segment" shortened to roughly `max` characters. */
export function shortUrl(url, max = 56) {
  const u = parseHttpUrl(url);
  if (!u) return String(url || '').slice(0, max);
  const name = basename(u.pathname) || '/';
  const full = `${u.host}${u.pathname}`;
  if (full.length <= max) return full;
  const tail = name.length > max - 4 ? `${name.slice(0, max - 6)}…` : name;
  const room = Math.max(4, max - tail.length - 2);
  return `${u.host.slice(0, room)}/…/${tail}`;
}
