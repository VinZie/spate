// Spate background: download capture, context menus, media detection and
// the message API used by the popup and the options page.
//
// Chromium runs this file as an MV3 module service worker; build.mjs bundles
// it (with its imports) into one classic script for Firefox's event page.
// Every listener is registered synchronously at top level so that events
// can wake the worker / event page.

import { api, actionApi, call, isFirefox, sessionArea } from './lib/compat.js';
import {
  baseMime,
  basename,
  buildCookieHeader,
  classifyMedia,
  cleanItem,
  downloadKind,
  fileNameFromUrl,
  formatSize,
  headerMap,
  hostOf,
  mediaFileName,
  mediaKey,
  mediaLabel,
  mediaSupported,
  normalizeSettings,
  parseHttpUrl,
  pickBestMedia,
  positiveInt,
  responseSize,
  sanitizeFileName,
  selectCookies,
  shortUrl,
  shouldCapture,
} from './lib/core.js';
import { loadSettings, onSettingsChanged } from './lib/settings.js';
import { pingHost, sendToHost } from './lib/native.js';
import { addTabMedia, clearTabMedia, forgetTabMedia, getTabMedia } from './lib/media-store.js';
import { collectPageLinks } from './lib/page-scripts.js';

const MENU_LINK = 'spate-link';
const MENU_DOWNLOADS_PAGE = 'spate-downloads-page';
const MENU_ALL_LINKS = 'spate-all-links';
const MENU_SELECTED_LINKS = 'spate-selected-links';
const MENU_MEDIA = 'spate-media';
const MENU_MEDIA_CHILD = 'spate-media:';
const MEDIA_MENU_LIMIT = 10;
const MEDIA_MENU_CONTEXTS = ['page', 'frame', 'video', 'audio', 'link', 'image'];
const WEB_PAGES = ['http://*/*', 'https://*/*', 'file:///*'];
const WEB_REQUESTS = ['http://*/*', 'https://*/*'];
const MEDIA_REQUEST_TYPES = ['main_frame', 'sub_frame', 'object', 'xmlhttprequest', 'media', 'other'];
const ERROR_KEY = 'lastError';
const BADGE_COLORS = { normal: '#2563eb', off: '#6b7280', error: '#dc2626' };
const ICONS_ON = { 16: '/icons/icon-16.png', 32: '/icons/icon-32.png' };
const ICONS_OFF = { 16: '/icons/icon-off-16.png', 32: '/icons/icon-off-32.png' };

let settings = normalizeSettings(undefined);
let lastError = null; // {message, at}
const handledDownloads = new Set();

const ready = Promise.all([
  loadSettings().then((s) => {
    settings = s;
  }),
  loadLastError(),
]).then(() => refreshGlobalBadge());

function currentSettings() {
  return ready.then(() => settings, () => settings);
}

// ---------------------------------------------------------------------------
// Badge, icon and error state
// ---------------------------------------------------------------------------

async function loadLastError() {
  const area = sessionArea();
  if (!area) return;
  try {
    const data = await call(area, 'get', ERROR_KEY);
    if (data && data[ERROR_KEY] && typeof data[ERROR_KEY].message === 'string') lastError = data[ERROR_KEY];
  } catch {
    /* ignore */
  }
}

async function setLastError(message) {
  lastError = { message: String(message), at: Date.now() };
  const area = sessionArea();
  if (area) await call(area, 'set', { [ERROR_KEY]: lastError }).catch(() => {});
  await refreshGlobalBadge();
}

async function clearLastError() {
  if (!lastError) return;
  lastError = null;
  const area = sessionArea();
  if (area) await call(area, 'remove', ERROR_KEY).catch(() => {});
  await refreshGlobalBadge();
}

async function refreshGlobalBadge() {
  if (!actionApi) return;
  let text = '';
  let color = BADGE_COLORS.normal;
  let title = 'Spate';
  if (lastError) {
    text = '!';
    color = BADGE_COLORS.error;
    title = `Spate: ${lastError.message}`;
  } else if (!settings.enabled) {
    text = 'off';
    color = BADGE_COLORS.off;
    title = 'Spate: download capture is off';
  }
  await Promise.allSettled([
    call(actionApi, 'setBadgeText', { text }),
    call(actionApi, 'setBadgeBackgroundColor', { color }),
    call(actionApi, 'setTitle', { title }),
    call(actionApi, 'setIcon', { path: settings.enabled ? ICONS_ON : ICONS_OFF }),
  ]);
}

/** Per-tab media count; `null` text falls back to the global badge. */
async function setTabBadge(tabId, count) {
  if (!actionApi) return;
  try {
    await call(actionApi, 'setBadgeText', { tabId, text: count > 0 ? String(Math.min(count, 99)) : null });
  } catch {
    if (!count) await call(actionApi, 'setBadgeText', { tabId, text: '' }).catch(() => {});
  }
}

// ---------------------------------------------------------------------------
// Building download items
// ---------------------------------------------------------------------------

/** Cookie header the browser would send for `url` from a page on `referrer`. */
async function cookieHeaderFor(url, { referrer, incognito, cookieStoreId } = {}) {
  if (!api.cookies) return '';
  const query = { url };
  if (cookieStoreId) query.storeId = cookieStoreId;
  else if (incognito && !isFirefox) query.storeId = '1';
  if (isFirefox) query.firstPartyDomain = null;
  let cookies;
  try {
    // `partitionKey: {}` asks for partitioned (CHIPS) cookies too where supported.
    cookies = await call(api.cookies, 'getAll', { ...query, partitionKey: {} });
  } catch {
    cookies = await call(api.cookies, 'getAll', query).catch(() => []);
  }
  return buildCookieHeader(selectCookies(cookies, hostOf(referrer) || hostOf(url)));
}

/** A `download` request item in the native-host format (unknown fields omitted). */
async function describeDownload({ url, fileName, referrer, mime, size, incognito, cookieStoreId, kind, cookies }) {
  const cookieHeader = cookies !== undefined ? cookies : await cookieHeaderFor(url, { referrer, incognito, cookieStoreId });
  return cleanItem({
    url,
    fileName: fileName ? sanitizeFileName(fileName) : undefined,
    referrer: parseHttpUrl(referrer) ? referrer : undefined,
    cookies: cookieHeader,
    userAgent: globalThis.navigator ? navigator.userAgent : undefined,
    fileSize: positiveInt(size),
    mime: baseMime(mime) || undefined,
    kind: kind || downloadKind(url, mime, fileName),
  });
}

async function sendItems(items, interactive, what) {
  const reply = await sendToHost({ type: 'download', interactive: Boolean(interactive), items });
  if (reply.ok) await clearLastError();
  else await setLastError(`${what || 'The download'} was not sent to Spate: ${reply.error}`);
  return reply;
}

// ---------------------------------------------------------------------------
// 1. Browser download capture
// ---------------------------------------------------------------------------

// The downloads API does not expose the request method, so remember recent
// form submissions (POST navigations): their responses cannot be re-fetched.
const POST_MEMORY_MS = 120000;
const recentPosts = new Map(); // url -> time

function rememberPost(details) {
  if (details.method !== 'POST') return;
  recentPosts.delete(details.url);
  recentPosts.set(details.url, Date.now());
  if (recentPosts.size > 200) recentPosts.delete(recentPosts.keys().next().value);
}

function wasPosted(...urls) {
  const now = Date.now();
  return urls.some((u) => u && recentPosts.has(u) && now - recentPosts.get(u) < POST_MEMORY_MS);
}

/**
 * Hand a browser download to Spate when the settings say so. `release` lets
 * Chromium continue its filename determination; it is called as soon as the
 * outcome is known.
 */
async function handleBrowserDownload(item, release) {
  const done = () => {
    if (release) release();
  };
  if (!item || handledDownloads.has(item.id)) return done();
  handledDownloads.add(item.id);
  const s = await currentSettings();
  const url = item.finalUrl || item.url;
  const fileName = sanitizeFileName(basename(item.filename || ''));
  const size = positiveInt(item.fileSize) ?? positiveInt(item.totalBytes);
  const method = wasPosted(item.url, item.finalUrl) ? 'POST' : 'GET';
  const decision = shouldCapture(
    { url, referrer: item.referrer, fileName, mime: item.mime, size, byExtensionId: item.byExtensionId, state: item.state, method },
    s,
  );
  if (!decision.capture) return done();

  const paused = await call(api.downloads, 'pause', item.id).then(() => true, () => false);
  if (!paused) {
    // Too late to stop it (e.g. a tiny file already finished): leave it alone.
    const [now] = await call(api.downloads, 'search', { id: item.id }).catch(() => []);
    if (!now || now.state !== 'in_progress') return done();
  }
  const payload = await describeDownload({
    url,
    fileName,
    referrer: item.referrer,
    mime: item.mime,
    size,
    incognito: item.incognito,
    cookieStoreId: item.cookieStoreId,
  });
  const reply = await sendToHost({ type: 'download', interactive: s.interactive, items: [payload] });
  if (reply.ok) {
    await call(api.downloads, 'cancel', item.id).catch(() => {});
    done();
    await call(api.downloads, 'erase', { id: item.id }).catch(() => {});
    await clearLastError();
  } else {
    if (paused) await call(api.downloads, 'resume', item.id).catch(() => {});
    done();
    await setLastError(`${fileName || shortUrl(url)} was left to the browser: ${reply.error}`);
  }
}

if (api.downloads) {
  if (api.downloads.onDeterminingFilename && !isFirefox) {
    // Chromium: the item now carries the final suggested file name, and the
    // download waits for our answer, so no "Save as" dialog appears early.
    api.downloads.onDeterminingFilename.addListener((item, suggest) => {
      let released = false;
      const release = () => {
        if (released) return;
        released = true;
        try {
          suggest();
        } catch {
          /* download already gone */
        }
      };
      handleBrowserDownload(item, release).catch(() => {}).finally(release);
      return true; // we call suggest() asynchronously
    });
  } else {
    api.downloads.onCreated.addListener((item) => {
      handleBrowserDownload(item, null).catch(() => {});
    });
  }
}

// ---------------------------------------------------------------------------
// 2. Context menus
// ---------------------------------------------------------------------------

let menuQueue = Promise.resolve();
function menuOp(fn) {
  const p = menuQueue.then(fn, fn);
  menuQueue = p.catch(() => {});
  return p;
}

function menuTitle(text) {
  // Firefox treats "&" as an access-key marker.
  return isFirefox ? text.replace(/&/g, '&&') : text;
}

function createMenu(props) {
  return new Promise((resolve, reject) => {
    try {
      api.contextMenus.create(props, () => {
        const err = api.runtime.lastError;
        if (err) reject(new Error(err.message));
        else resolve(props.id);
      });
    } catch (e) {
      reject(e);
    }
  });
}

let mediaMenuChildren = 0;

function setupMenus() {
  return menuOp(async () => {
    await call(api.contextMenus, 'removeAll').catch(() => {});
    mediaMenuChildren = 0;
    await createMenu({
      id: MENU_LINK,
      title: 'Download with Spate',
      contexts: ['link', 'image', 'video', 'audio', 'frame'],
      documentUrlPatterns: WEB_PAGES,
    });
    await createMenu({
      id: MENU_SELECTED_LINKS,
      title: 'Download selected links with Spate',
      contexts: ['selection'],
      documentUrlPatterns: WEB_PAGES,
    });
    await createMenu({
      id: MENU_ALL_LINKS,
      title: 'Download all links with Spate',
      contexts: ['page', 'frame'],
      documentUrlPatterns: WEB_PAGES,
    });
    await createMenu({
      id: MENU_MEDIA,
      title: 'Download this video/stream with Spate',
      contexts: MEDIA_MENU_CONTEXTS,
      documentUrlPatterns: WEB_PAGES,
      visible: false,
    });
    if (!isFirefox) {
      // Links on the browser's own downloads page: re-download with Spate.
      await createMenu({
        id: MENU_DOWNLOADS_PAGE,
        title: 'Download again with Spate',
        contexts: ['link'],
        documentUrlPatterns: ['chrome://downloads/*'],
      }).catch(() => {});
    }
  });
}

/** Re-create the menus only if they are missing (they persist across restarts). */
async function ensureMenus() {
  try {
    await call(api.contextMenus, 'update', MENU_LINK, { title: 'Download with Spate' });
  } catch {
    await setupMenus();
  }
}

/** Menu order of a tab's media: playable entries first, newest first. */
function mediaMenuOrder(list) {
  return list.filter(mediaSupported).concat(list.filter((m) => !mediaSupported(m)));
}

function mediaMenuItemTitle(m) {
  const name = fileNameFromUrl(m.url) || shortUrl(m.url, 40);
  const size = m.kind === 'hls' ? '' : formatSize(m.size);
  const extra = mediaSupported(m) ? '' : ' (not supported)';
  return menuTitle(`${mediaLabel(m)}: ${name}${size ? ` (${size})` : ''}${extra}`);
}

function mediaNoun(m) {
  if (m.kind === 'hls') return 'stream';
  return m.kind === 'audio' ? 'audio' : 'video';
}

async function activeTabId() {
  const tabs = await call(api.tabs, 'query', { active: true, lastFocusedWindow: true }).catch(() => []);
  return tabs && tabs[0] ? tabs[0].id : undefined;
}

let mediaMenuTimer = null;
function scheduleMediaMenu(tabId) {
  clearTimeout(mediaMenuTimer);
  mediaMenuTimer = setTimeout(() => {
    refreshMediaMenu(tabId).catch(() => {});
  }, 150);
}

async function refreshMediaMenu(tabId) {
  const id = tabId ?? (await activeTabId());
  const list = id === undefined ? [] : mediaMenuOrder(await getTabMedia(id));
  await menuOp(async () => {
    for (let i = 0; i < mediaMenuChildren; i += 1) {
      await call(api.contextMenus, 'remove', `${MENU_MEDIA_CHILD}${i}`).catch(() => {});
    }
    mediaMenuChildren = 0;
    if (!list.length) {
      await call(api.contextMenus, 'update', MENU_MEDIA, { visible: false }).catch(() => {});
      return;
    }
    if (list.length === 1) {
      const m = list[0];
      await call(api.contextMenus, 'update', MENU_MEDIA, {
        visible: true,
        enabled: mediaSupported(m),
        title: mediaSupported(m) ? `Download this ${mediaNoun(m)} with Spate` : 'Detected DASH stream (not supported)',
      }).catch(() => {});
      return;
    }
    await call(api.contextMenus, 'update', MENU_MEDIA, {
      visible: true,
      enabled: true,
      title: `Download this video/stream with Spate (${list.length})`,
    }).catch(() => {});
    const shown = list.slice(0, MEDIA_MENU_LIMIT);
    for (let i = 0; i < shown.length; i += 1) {
      await createMenu({
        id: `${MENU_MEDIA_CHILD}${i}`,
        parentId: MENU_MEDIA,
        title: mediaMenuItemTitle(shown[i]),
        contexts: MEDIA_MENU_CONTEXTS,
        enabled: mediaSupported(shown[i]),
      }).catch(() => {});
      mediaMenuChildren = i + 1;
    }
  });
}

async function onMenuClicked(info, tab) {
  const id = String(info.menuItemId);
  const s = await currentSettings();
  if (id === MENU_LINK || id === MENU_DOWNLOADS_PAGE) {
    const target = info.linkUrl || info.srcUrl || info.frameUrl;
    if (!parseHttpUrl(target)) {
      // A <video> fed by Media Source Extensions has a blob: URL; offer the
      // stream detected on the page instead.
      if (tab && (info.mediaType === 'video' || info.mediaType === 'audio')) {
        const best = pickBestMedia(await getTabMedia(tab.id));
        if (best) return downloadMedia(tab.id, best, tab);
      }
      return setLastError('That item has no http(s) address Spate could download');
    }
    const referrer = id === MENU_DOWNLOADS_PAGE ? undefined : info.frameUrl || info.pageUrl;
    const item = await describeDownload({
      url: target,
      referrer,
      incognito: tab && tab.incognito,
      cookieStoreId: tab && tab.cookieStoreId,
    });
    return sendItems([item], s.interactive, shortUrl(target));
  }
  if (id === MENU_ALL_LINKS || id === MENU_SELECTED_LINKS) {
    if (!tab) return undefined;
    return downloadPageLinks(tab, info.frameId || 0, id === MENU_SELECTED_LINKS);
  }
  if (id === MENU_MEDIA && tab) {
    const best = pickBestMedia(await getTabMedia(tab.id));
    if (best) return downloadMedia(tab.id, best, tab);
    return setLastError('No downloadable video or audio was detected on this page');
  }
  if (id.startsWith(MENU_MEDIA_CHILD) && tab) {
    const index = Number(id.slice(MENU_MEDIA_CHILD.length));
    const m = mediaMenuOrder(await getTabMedia(tab.id))[index];
    if (m) return downloadMedia(tab.id, m, tab);
  }
  return undefined;
}

if (api.contextMenus) {
  api.contextMenus.onClicked.addListener((info, tab) => {
    onMenuClicked(info, tab).catch((e) => setLastError(e && e.message ? e.message : String(e)));
  });
  // Firefox can update a menu right before it is shown.
  if (api.contextMenus.onShown && typeof api.contextMenus.refresh === 'function') {
    api.contextMenus.onShown.addListener((info, tab) => {
      if (!tab || tab.id === undefined || tab.id < 0) return;
      refreshMediaMenu(tab.id)
        .then(() => call(api.contextMenus, 'refresh'))
        .catch(() => {});
    });
  }
}

// ---------------------------------------------------------------------------
// Batch: all links / selected links
// ---------------------------------------------------------------------------

/** Run `fn` over `items` with at most `limit` calls in flight. */
async function mapLimit(items, limit, fn) {
  const out = new Array(items.length);
  let next = 0;
  async function worker() {
    while (next < items.length) {
      const i = next;
      next += 1;
      out[i] = await fn(items[i], i);
    }
  }
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
  return out;
}

async function downloadPageLinks(tab, frameId, selectionOnly) {
  let results;
  try {
    results = await call(api.scripting, 'executeScript', {
      target: { tabId: tab.id, frameIds: [frameId] },
      func: collectPageLinks,
      args: [Boolean(selectionOnly)],
    });
  } catch (e) {
    const error = `Cannot read links from this page: ${e && e.message ? e.message : e}`;
    await setLastError(error);
    return { ok: false, error };
  }
  const data = results && results[0] && results[0].result;
  if (!data || !Array.isArray(data.links) || !data.links.length) {
    const error = selectionOnly ? 'No links found in the selection' : 'No links found on this page';
    await setLastError(error);
    return { ok: false, error };
  }
  // Cookies depend on host and path; links in one directory share them.
  const cookieCache = new Map();
  const cookiesFor = (url) => {
    const u = new URL(url);
    const key = `${u.origin}${u.pathname.replace(/[^/]*$/, '')}`;
    if (!cookieCache.has(key)) {
      cookieCache.set(key, cookieHeaderFor(url, { referrer: data.pageUrl, incognito: tab.incognito, cookieStoreId: tab.cookieStoreId }));
    }
    return cookieCache.get(key);
  };
  const items = await mapLimit(data.links, 8, async (link) =>
    describeDownload({
      url: link.url,
      fileName: link.fileName,
      referrer: data.pageUrl,
      cookies: await cookiesFor(link.url),
    }),
  );
  const reply = await sendItems(items, true, `${items.length} link${items.length === 1 ? '' : 's'}`);
  return { ...reply, count: items.length };
}

// ---------------------------------------------------------------------------
// 3. Media detection
// ---------------------------------------------------------------------------

async function downloadMedia(tabId, media, tabHint) {
  if (!mediaSupported(media)) {
    return { ok: false, error: 'DASH (.mpd) streams are not supported by Spate yet' };
  }
  const tab = (await call(api.tabs, 'get', tabId).catch(() => null)) || tabHint || {};
  const referrer = parseHttpUrl(tab.url) ? tab.url : media.pageUrl;
  const isHls = media.kind === 'hls';
  const item = await describeDownload({
    url: media.url,
    fileName: mediaFileName(media, tab.title),
    referrer,
    mime: media.mime,
    size: isHls ? undefined : media.size, // a playlist's size says nothing about the stream
    incognito: tab.incognito,
    cookieStoreId: tab.cookieStoreId,
    kind: isHls ? 'hls' : 'http',
  });
  const s = await currentSettings();
  return sendItems([item], s.interactive, shortUrl(media.url));
}

function onMediaHeaders(details) {
  if (details.tabId === undefined || details.tabId < 0 || !settings.detectMedia) return;
  const headers = headerMap(details.responseHeaders);
  const media = classifyMedia({
    url: details.url,
    contentType: headers['content-type'],
    size: responseSize(headers),
    statusCode: details.statusCode,
  });
  if (!media) return;
  const tabId = details.tabId;
  const entry = {
    url: details.url,
    key: mediaKey(details.url),
    kind: media.kind,
    mime: media.mime,
    ext: media.ext,
    size: media.size,
    pageUrl: details.documentUrl || details.originUrl || details.initiator || null,
    detectedAt: Date.now(),
  };
  addTabMedia(tabId, entry)
    .then(async ({ added, count }) => {
      if (!added) return;
      await setTabBadge(tabId, count);
      if (tabId === (await activeTabId())) scheduleMediaMenu(tabId);
    })
    .catch(() => {});
}

if (api.webRequest) {
  try {
    api.webRequest.onHeadersReceived.addListener(onMediaHeaders, { urls: WEB_REQUESTS, types: MEDIA_REQUEST_TYPES }, ['responseHeaders']);
  } catch {
    api.webRequest.onHeadersReceived.addListener(onMediaHeaders, { urls: WEB_REQUESTS }, ['responseHeaders']);
  }
  api.webRequest.onBeforeRequest.addListener(rememberPost, { urls: WEB_REQUESTS, types: ['main_frame', 'sub_frame'] });
}

if (api.webNavigation) {
  api.webNavigation.onCommitted.addListener((details) => {
    if (details.frameId !== 0 || details.tabId < 0) return;
    if (details.documentLifecycle && details.documentLifecycle !== 'active') return; // prerender
    clearTabMedia(details.tabId, details.url)
      .then(async ({ count }) => {
        await setTabBadge(details.tabId, count);
        if (details.tabId === (await activeTabId())) scheduleMediaMenu(details.tabId);
      })
      .catch(() => {});
  });
}

if (api.tabs) {
  api.tabs.onRemoved.addListener((tabId) => {
    forgetTabMedia(tabId).catch(() => {});
  });
  api.tabs.onActivated.addListener(({ tabId }) => scheduleMediaMenu(tabId));
  if (api.tabs.onReplaced) api.tabs.onReplaced.addListener((added, removed) => forgetTabMedia(removed).catch(() => {}));
}
if (api.windows && api.windows.onFocusChanged) {
  api.windows.onFocusChanged.addListener(() => scheduleMediaMenu(undefined));
}

// ---------------------------------------------------------------------------
// 4. Messages from the popup and the options page
// ---------------------------------------------------------------------------

async function handleMessage(msg) {
  switch (msg.cmd) {
    case 'popup-state': {
      const s = await currentSettings();
      const media = msg.tabId === undefined ? [] : mediaMenuOrder(await getTabMedia(msg.tabId));
      return { ok: true, settings: s, media, lastError, version: api.runtime.getManifest().version };
    }
    case 'ping':
      return pingHost();
    case 'download-media': {
      const m = (await getTabMedia(msg.tabId)).find((x) => x.key === msg.key);
      if (!m) return { ok: false, error: 'That media entry is gone; reload the page' };
      return downloadMedia(msg.tabId, m);
    }
    case 'download-links': {
      const tab = await call(api.tabs, 'get', msg.tabId).catch(() => null);
      if (!tab) return { ok: false, error: 'No active tab' };
      return downloadPageLinks(tab, 0, false);
    }
    case 'clear-media': {
      await clearTabMedia(msg.tabId);
      await setTabBadge(msg.tabId, 0);
      scheduleMediaMenu(msg.tabId);
      return { ok: true };
    }
    case 'clear-error':
      await clearLastError();
      return { ok: true };
    default:
      return { ok: false, error: `Unknown command ${msg.cmd}` };
  }
}

api.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  if (!msg || typeof msg.cmd !== 'string' || !sender || sender.id !== api.runtime.id) return false;
  handleMessage(msg).then(sendResponse, (e) => sendResponse({ ok: false, error: e && e.message ? e.message : String(e) }));
  return true;
});

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

onSettingsChanged((next) => {
  settings = next;
  refreshGlobalBadge().catch(() => {});
});

api.runtime.onInstalled.addListener((details) => {
  setupMenus()
    .then(() => refreshMediaMenu())
    .catch(() => {});
  if (details && details.reason === 'install') call(api.runtime, 'openOptionsPage').catch(() => {});
});

if (api.contextMenus) {
  ensureMenus()
    .then(() => refreshMediaMenu())
    .catch(() => {});
}
