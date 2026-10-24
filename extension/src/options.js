import { api, call } from './lib/compat.js';
import { NATIVE_HOST, normalizeSettings, parseExtensionList, parseSiteList } from './lib/core.js';
import { loadSettings, resetSettings, saveSettings } from './lib/settings.js';

const form = document.getElementById('form');
const $ = (id) => document.getElementById(id);
const BOOLEAN_FIELDS = ['enabled', 'interactive', 'captureAll', 'detectMedia'];

function fill(settings) {
  for (const name of BOOLEAN_FIELDS) form.elements[name].checked = settings[name];
  form.elements.extensions.value = settings.extensions.join(' ');
  form.elements.minSizeKB.value = String(settings.minSizeKB);
  form.elements.excludedSites.value = settings.excludedSites.join('\n');
  syncDisabled();
}

function read() {
  const out = {};
  for (const name of BOOLEAN_FIELDS) out[name] = form.elements[name].checked;
  out.extensions = parseExtensionList(form.elements.extensions.value);
  out.minSizeKB = Number(form.elements.minSizeKB.value || 0);
  out.excludedSites = parseSiteList(form.elements.excludedSites.value.split('\n'));
  return normalizeSettings(out);
}

function syncDisabled() {
  form.elements.extensions.disabled = form.elements.captureAll.checked;
}

let savedTimer;
function flash(text) {
  $('saved').textContent = text;
  clearTimeout(savedTimer);
  savedTimer = setTimeout(() => {
    $('saved').textContent = '';
  }, 2500);
}

form.elements.captureAll.addEventListener('change', syncDisabled);

form.addEventListener('submit', async (e) => {
  e.preventDefault();
  try {
    fill(await saveSettings(read()));
    flash('Saved.');
  } catch (err) {
    flash(`Could not save: ${err.message}`);
  }
});

$('reset').addEventListener('click', async () => {
  fill(await resetSettings());
  flash('Defaults restored.');
});

function code(text) {
  const el = document.createElement('code');
  el.textContent = text;
  return el;
}

$('test').addEventListener('click', async () => {
  const status = $('status');
  const text = $('status-text');
  status.className = 'status';
  text.textContent = 'Testing…';
  const r = await call(api.runtime, 'sendMessage', { cmd: 'ping' }).catch((e) => ({ ok: false, error: e.message }));
  if (r.ok) {
    status.className = 'status ok';
    text.textContent = `Connected to Spate v${r.version || '?'}.`;
  } else if (r.errorKind === 'not-installed') {
    status.className = 'status error';
    text.replaceChildren('Native host not installed — run ', code('spate-cli install-host'), ' and restart the browser if needed.');
  } else if (r.errorKind === 'app-not-running') {
    status.className = 'status warn';
    text.textContent = 'The native host works, but the Spate app is not running.';
  } else {
    status.className = 'status error';
    text.textContent = `Failed: ${r.error}`;
  }
});

$('host-name').textContent = NATIVE_HOST;
$('extension-id').textContent = api.runtime.id;

loadSettings().then(fill);
