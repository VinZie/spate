// User settings, kept in storage.local under one key.

import { api, call } from './compat.js';
import { normalizeSettings } from './core.js';

export const SETTINGS_KEY = 'settings';

export async function loadSettings() {
  try {
    const data = await call(api.storage.local, 'get', SETTINGS_KEY);
    return normalizeSettings(data && data[SETTINGS_KEY]);
  } catch {
    return normalizeSettings(undefined);
  }
}

/** Merge `partial` into the stored settings and return the result. */
export async function saveSettings(partial) {
  const current = await loadSettings();
  const next = normalizeSettings({ ...current, ...partial });
  await call(api.storage.local, 'set', { [SETTINGS_KEY]: next });
  return next;
}

export async function resetSettings() {
  const next = normalizeSettings(undefined);
  await call(api.storage.local, 'set', { [SETTINGS_KEY]: next });
  return next;
}

/** Run `callback(settings)` whenever another context changes the settings. */
export function onSettingsChanged(callback) {
  api.storage.onChanged.addListener((changes, area) => {
    if (area === 'local' && changes[SETTINGS_KEY]) callback(normalizeSettings(changes[SETTINGS_KEY].newValue));
  });
}
