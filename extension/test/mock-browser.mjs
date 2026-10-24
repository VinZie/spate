// A small stand-in for the `browser` / `chrome` extension namespaces.
//
// Every property path is auto-created. Calls are recorded in `calls`
// (`[path, args]`), event listeners in `listeners[path]`. Results come from
// `responses[path]` (a value, an Error to simulate failure, or a function of
// the call arguments). In callback mode (Chromium style) a failure is
// reported through `runtime.lastError` inside the callback; in promise mode
// (Firefox style) the returned promise rejects.

const SYNC_METHODS = new Set(['runtime.getManifest', 'runtime.getURL', 'contextMenus.create']);

export function createMockBrowser({ promises = true, omit = [], responses = {}, id = 'test-extension-id' } = {}) {
  const calls = [];
  const listeners = {};
  const state = { lastError: undefined };

  const resolveResponse = (path, args) => {
    const r = responses[path];
    return typeof r === 'function' ? r(...args) : r;
  };

  function node(path, isRoot) {
    const cache = new Map();
    const target = isRoot ? {} : function mockMethod() {};
    const handler = {
      get(_t, prop) {
        if (typeof prop === 'symbol' || prop === 'then') return undefined;
        const full = path ? `${path}.${prop}` : prop;
        if (omit.includes(full)) return undefined;
        if (full === 'runtime.lastError') return state.lastError;
        if (full === 'runtime.id') return id;
        if (prop === 'addListener') {
          return (fn) => {
            (listeners[path] ||= []).push(fn);
          };
        }
        if (!cache.has(prop)) cache.set(prop, node(full, false));
        return cache.get(prop);
      },
    };
    if (!isRoot) {
      handler.apply = (_t, _this, args) => {
        const callback = typeof args[args.length - 1] === 'function' ? args[args.length - 1] : null;
        calls.push([path, callback ? args.slice(0, -1) : args]);
        let result;
        let error;
        try {
          result = resolveResponse(path, callback ? args.slice(0, -1) : args);
        } catch (e) {
          error = e;
        }
        if (result instanceof Error) {
          error = result;
          result = undefined;
        }
        if (SYNC_METHODS.has(path)) {
          if (path === 'contextMenus.create' && callback) {
            queueMicrotask(() => {
              state.lastError = error ? { message: error.message } : undefined;
              callback();
              state.lastError = undefined;
            });
            return args[0] && args[0].id;
          }
          if (error) throw error;
          return result;
        }
        if (promises && !callback) {
          return result instanceof Promise ? result : error ? Promise.reject(error) : Promise.resolve(result);
        }
        Promise.resolve(result).then(
          (value) => {
            if (!callback) return;
            state.lastError = error ? { message: error.message } : undefined;
            callback(error ? undefined : value);
            state.lastError = undefined;
          },
          (e) => {
            if (!callback) return;
            state.lastError = { message: e.message };
            callback(undefined);
            state.lastError = undefined;
          },
        );
        return undefined;
      };
    }
    return new Proxy(target, handler);
  }

  const api = node('', true);
  // Arguments are re-created in this realm so deepStrictEqual works on values
  // produced inside a vm context (whose Object/Array prototypes differ).
  const callsTo = (path) => calls.filter(([p]) => p === path).map(([, args]) => toPlain(args));
  const fire = (path, ...args) => (listeners[path] || []).map((fn) => fn(...args));
  return { api, calls, callsTo, listeners, fire, responses };
}

/** Deep copy into this realm (functions are kept by reference). */
export function toPlain(value) {
  // Array.from (not .map, which would use the source realm's Array species).
  if (Array.isArray(value)) return Array.from(value, toPlain);
  if (value && typeof value === 'object') {
    const out = {};
    for (const [k, v] of Object.entries(value)) out[k] = toPlain(v);
    return out;
  }
  return value;
}

/** Let pending promise chains and short timers run. */
export async function settle(rounds = 6) {
  for (let i = 0; i < rounds; i += 1) await new Promise((r) => setTimeout(r, 5));
}
