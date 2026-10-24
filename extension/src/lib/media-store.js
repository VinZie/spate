// Per-tab list of detected media. Kept in memory and mirrored to
// storage.session so it survives MV3 service-worker restarts; falls back to
// memory only where storage.session does not exist.

import { call, sessionArea } from './compat.js';
import { mediaKey } from './core.js';

const MEDIA_PER_TAB = 60;
const mediaCache = new Map(); // tabId -> entries (oldest first)
let mediaQueue = Promise.resolve();

function mediaStorageKey(tabId) {
  return `media:${tabId}`;
}

/** Serialize store operations so concurrent events cannot lose updates. */
function mediaOp(fn) {
  const p = mediaQueue.then(fn, fn);
  mediaQueue = p.catch(() => {});
  return p;
}

async function loadTabMedia(tabId) {
  if (mediaCache.has(tabId)) return mediaCache.get(tabId);
  let list = [];
  const area = sessionArea();
  if (area) {
    try {
      const key = mediaStorageKey(tabId);
      const data = await call(area, 'get', key);
      if (data && Array.isArray(data[key])) list = data[key];
    } catch {
      /* memory only */
    }
  }
  mediaCache.set(tabId, list);
  return list;
}

async function persistTabMedia(tabId) {
  const area = sessionArea();
  if (!area) return;
  const key = mediaStorageKey(tabId);
  const list = mediaCache.get(tabId) || [];
  try {
    if (list.length) await call(area, 'set', { [key]: list });
    else await call(area, 'remove', key);
  } catch {
    /* quota or unavailable: memory copy still works */
  }
}

/** Detected media of a tab, newest first. */
export function getTabMedia(tabId) {
  return mediaOp(async () => (await loadTabMedia(tabId)).slice().reverse());
}

/**
 * Record a detection. Returns {added, count}. A repeat of a known entry only
 * refreshes its size when a larger total is seen.
 */
export function addTabMedia(tabId, entry) {
  return mediaOp(async () => {
    const list = await loadTabMedia(tabId);
    const key = entry.key || mediaKey(entry.url);
    const existing = list.find((m) => m.key === key);
    if (existing) {
      if (entry.size && (!existing.size || entry.size > existing.size)) {
        existing.size = entry.size;
        await persistTabMedia(tabId);
      }
      return { added: false, count: list.length };
    }
    list.push({ ...entry, key });
    if (list.length > MEDIA_PER_TAB) list.splice(0, list.length - MEDIA_PER_TAB);
    await persistTabMedia(tabId);
    return { added: true, count: list.length };
  });
}

/**
 * Forget a tab's media (navigation). Entries whose URL equals `keepUrl` are
 * kept: that is the document itself, e.g. a video opened directly in a tab.
 */
export function clearTabMedia(tabId, keepUrl) {
  return mediaOp(async () => {
    const list = await loadTabMedia(tabId);
    const kept = keepUrl ? list.filter((m) => m.url === keepUrl) : [];
    const changed = kept.length !== list.length;
    mediaCache.set(tabId, kept);
    if (changed) await persistTabMedia(tabId);
    return { count: kept.length, changed };
  });
}

export function forgetTabMedia(tabId) {
  return mediaOp(async () => {
    mediaCache.delete(tabId);
    const area = sessionArea();
    if (area) await call(area, 'remove', mediaStorageKey(tabId)).catch(() => {});
  });
}
