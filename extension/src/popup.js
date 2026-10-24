import { api, call } from './lib/compat.js';
import { fileNameFromUrl, formatSize, mediaLabel, mediaSupported, shortUrl } from './lib/core.js';
import { saveSettings } from './lib/settings.js';

const HOST_ORIGINS = ['http://*/*', 'https://*/*'];
const $ = (id) => document.getElementById(id);
let tabId;

function send(msg) {
  return call(api.runtime, 'sendMessage', msg).catch((e) => ({ ok: false, error: e.message, errorKind: 'other' }));
}

function code(text) {
  const el = document.createElement('code');
  el.textContent = text;
  return el;
}

function setStatus(kind, parts) {
  $('status').className = `status ${kind}`;
  $('status-text').replaceChildren(...parts);
}

function showResult(text, isError = false) {
  const el = $('action-result');
  el.textContent = text;
  el.title = text;
  el.style.color = isError ? 'var(--error)' : '';
}

async function refreshStatus() {
  setStatus('', ['Checking connection to Spate…']);
  const r = await send({ cmd: 'ping' });
  if (r.ok) {
    setStatus('ok', [`Connected v${r.version || '?'}`]);
    return;
  }
  switch (r.errorKind) {
    case 'not-installed':
      setStatus('error', ['Native host not installed — run ', code('spate-cli install-host')]);
      break;
    case 'forbidden':
      setStatus('error', ['The native host does not list this extension. Run ', code('spate-cli install-host'), ' again.']);
      break;
    case 'app-not-running':
      setStatus('warn', ['Spate is not running. Start the Spate app, then reopen this popup.']);
      break;
    default:
      setStatus('error', [`Cannot reach Spate: ${r.error}`]);
  }
}

function renderError(lastError) {
  $('error').classList.toggle('hidden', !lastError);
  $('error-text').textContent = lastError ? lastError.message : '';
}

function renderMedia(list) {
  const ul = $('media-list');
  ul.replaceChildren();
  $('media-count').textContent = list.length ? `(${list.length})` : '';
  $('media-empty').classList.toggle('hidden', list.length > 0);
  $('clear-media').classList.toggle('hidden', list.length === 0);
  const template = $('media-row');
  for (const m of list) {
    const li = template.content.firstElementChild.cloneNode(true);
    li.querySelector('.tag').textContent = mediaLabel(m);
    const name = li.querySelector('.name');
    name.textContent = fileNameFromUrl(m.url) || shortUrl(m.url);
    name.title = m.url;
    const meta = [];
    if (m.kind === 'hls') meta.push('HLS stream');
    else if (m.kind === 'dash') meta.push('DASH stream');
    else if (formatSize(m.size)) meta.push(formatSize(m.size));
    meta.push(shortUrl(m.url, 44));
    li.querySelector('.meta').textContent = meta.join(' · ');
    const button = li.querySelector('.download');
    if (!mediaSupported(m)) {
      li.classList.add('unsupported');
      button.disabled = true;
      button.textContent = 'Unsupported';
      button.title = 'DASH (.mpd) streams are not supported yet';
    } else {
      button.addEventListener('click', async () => {
        button.disabled = true;
        button.textContent = 'Sending…';
        const r = await send({ cmd: 'download-media', tabId, key: m.key });
        if (r.ok) {
          button.textContent = 'Sent';
          showResult('Sent to Spate');
        } else {
          button.textContent = 'Download';
          button.disabled = false;
          showResult(r.error, true);
        }
      });
    }
    ul.append(li);
  }
}

async function checkHostPermission() {
  if (!api.permissions || !api.permissions.contains) return;
  const granted = await call(api.permissions, 'contains', { origins: HOST_ORIGINS }).catch(() => true);
  $('permission').classList.toggle('hidden', Boolean(granted));
}

async function init() {
  const tabs = await call(api.tabs, 'query', { active: true, currentWindow: true }).catch(() => []);
  const tab = tabs && tabs[0];
  tabId = tab ? tab.id : undefined;
  $('download-links').disabled = !(tab && /^https?:/.test(tab.url || ''));

  const state = await send({ cmd: 'popup-state', tabId });
  if (state.ok) {
    $('enabled').checked = state.settings.enabled;
    renderError(state.lastError);
    renderMedia(state.media || []);
  } else {
    renderMedia([]);
    showResult(state.error || 'Background page unavailable', true);
  }
  checkHostPermission();
  refreshStatus();
}

$('enabled').addEventListener('change', (e) => {
  saveSettings({ enabled: e.target.checked }).catch((err) => showResult(err.message, true));
});

$('error-dismiss').addEventListener('click', async () => {
  await send({ cmd: 'clear-error' });
  renderError(null);
});

$('clear-media').addEventListener('click', async () => {
  await send({ cmd: 'clear-media', tabId });
  renderMedia([]);
});

$('download-links').addEventListener('click', async () => {
  const button = $('download-links');
  button.disabled = true;
  showResult('Collecting links…');
  const r = await send({ cmd: 'download-links', tabId });
  button.disabled = false;
  if (r.ok) showResult(`Sent ${r.count} link${r.count === 1 ? '' : 's'} to Spate`);
  else showResult(r.error, true);
});

$('grant').addEventListener('click', () => {
  // Must run synchronously inside the click for the browser to show its prompt.
  call(api.permissions, 'request', { origins: HOST_ORIGINS })
    .then((ok) => {
      if (ok) $('permission').classList.add('hidden');
    })
    .catch((e) => showResult(e.message, true));
});

$('open-options').addEventListener('click', () => {
  call(api.runtime, 'openOptionsPage').catch(() => {});
  window.close();
});

init();
