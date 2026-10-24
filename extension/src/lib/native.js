// Talking to the Spate native messaging host (`spate-cli`), which forwards
// requests to the desktop app over its token-authenticated loopback IPC.
//
// Requests: {type: "ping"} and
//           {type: "download", interactive, items: [{url, fileName, referrer,
//            cookies, userAgent, fileSize, mime, kind}]}
// Reply:    {ok, error?, version}

import { api, call } from './compat.js';
import { NATIVE_HOST, nativeErrorKind } from './core.js';

export const NATIVE_TIMEOUT_MS = 30000;

function nativeFailure(error, version) {
  const message = String(error || 'Unknown native messaging error');
  return { ok: false, error: message, errorKind: nativeErrorKind(message), version: version || '' };
}

/**
 * Send one message to the host. Never rejects: failures come back as
 * {ok: false, error, errorKind} where errorKind is one of not-installed,
 * forbidden, app-not-running, timeout, host-error, app-error or other.
 */
export async function sendToHost(message, timeoutMs = NATIVE_TIMEOUT_MS) {
  let timer;
  try {
    const reply = await Promise.race([
      call(api.runtime, 'sendNativeMessage', NATIVE_HOST, message),
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error('Timed out waiting for the Spate native host')), timeoutMs);
      }),
    ]);
    if (!reply || typeof reply !== 'object') return nativeFailure('The Spate native host sent an empty reply');
    const version = typeof reply.version === 'string' ? reply.version : '';
    if (reply.ok === true) return { ok: true, version };
    const failure = nativeFailure(reply.error || 'Spate rejected the request', version);
    if (failure.errorKind === 'other') failure.errorKind = 'app-error';
    return failure;
  } catch (e) {
    return nativeFailure(e && e.message ? e.message : e);
  } finally {
    clearTimeout(timer);
  }
}

export function pingHost() {
  return sendToHost({ type: 'ping' }, 10000);
}
