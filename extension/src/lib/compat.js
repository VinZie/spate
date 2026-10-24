// Browser API compatibility shim.
//
// Firefox exposes promise-based `browser.*`; Chromium exposes `chrome.*`
// whose methods take callbacks (most also return promises in MV3, but not
// all of them, e.g. contextMenus.create). `call()` always returns a promise
// and turns `runtime.lastError` into a rejection, so the rest of the code can
// use async/await on both engines.

export const api = globalThis.browser ?? globalThis.chrome;

/** True when `api` is the promise-based `browser` namespace. */
export const promiseApi = typeof globalThis.browser === 'object' && globalThis.browser !== null;

export const isFirefox = typeof globalThis.browser?.runtime?.getBrowserInfo === 'function';

/** Toolbar button API (`action` in MV3, `browserAction` as a fallback). */
export const actionApi = api ? api.action || api.browserAction : undefined;

/**
 * Invoke `target[method](...args)` and get a promise for the result.
 * Rejects when the method is missing, throws, or sets runtime.lastError.
 */
export function call(target, method, ...args) {
  if (!target || typeof target[method] !== 'function') {
    return Promise.reject(new Error(`${method}() is not available in this browser`));
  }
  if (promiseApi) {
    try {
      return Promise.resolve(target[method](...args));
    } catch (e) {
      return Promise.reject(e);
    }
  }
  return new Promise((resolve, reject) => {
    try {
      target[method](...args, (result) => {
        const err = api.runtime && api.runtime.lastError;
        if (err) reject(new Error(err.message || String(err)));
        else resolve(result);
      });
    } catch (e) {
      reject(e);
    }
  });
}

/** `storage.session` when available (Chromium 102+, Firefox 115+). */
export function sessionArea() {
  return (api && api.storage && api.storage.session) || null;
}
