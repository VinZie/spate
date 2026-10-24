// End-to-end checks of the background logic against a mocked browser API:
// the Chromium build (ES modules, chrome.* callbacks, onDeterminingFilename)
// and the Firefox build (bundled classic script, browser.* promises, onCreated).

import assert from 'node:assert/strict';
import { join } from 'node:path';
import { describe, it } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import vm from 'node:vm';
import { bundleModules } from '../build.mjs';
import { createMockBrowser, settle, toPlain } from './mock-browser.mjs';

const ROOT = join(fileURLToPath(import.meta.url), '..', '..');

function baseResponses(extra = {}) {
  const menus = new Set();
  return {
    'contextMenus.create': (props) => {
      menus.add(props.id);
    },
    'contextMenus.update': (id) => (menus.has(id) ? undefined : new Error(`Cannot find menu item with id ${id}`)),
    'contextMenus.removeAll': () => {
      menus.clear();
    },
    'storage.local.get': {},
    'storage.session.get': {},
    'tabs.query': [{ id: 7 }],
    'tabs.get': { id: 7, url: 'https://videos.example/watch?v=1', title: 'A video: part 1' },
    'cookies.getAll': (q) => (q.url.startsWith('https://files.example/') ? [{ name: 'sid', value: 'abc', path: '/' }] : []),
    'downloads.search': [{ id: 1, state: 'in_progress' }],
    'runtime.getManifest': { version: '0.1.0' },
    'runtime.sendNativeMessage': (host, msg) => ({ ok: true, version: '9.9.9', echo: [host, msg] }),
    ...extra,
  };
}

function nativeMessages(mock) {
  return mock.callsTo('runtime.sendNativeMessage').map(([host, msg]) => {
    assert.equal(host, 'com.spate.host');
    return msg;
  });
}

function message(mock, msg) {
  return new Promise((resolve) => {
    const [listener] = mock.listeners['runtime.onMessage'];
    const keepOpen = listener(msg, { id: 'test-extension-id' }, (reply) => resolve(toPlain(reply)));
    assert.equal(keepOpen, true);
  });
}

function loadFirefoxBundle(mock) {
  const code = bundleModules(join(ROOT, 'src', 'background.js'));
  const context = vm.createContext({
    browser: mock.api,
    URL,
    setTimeout,
    clearTimeout,
    queueMicrotask,
    console,
    navigator: { userAgent: 'TestAgent/1.0' },
  });
  new vm.Script(code, { filename: 'background.bundle.js' }).runInContext(context);
}

const zipItem = {
  id: 1,
  url: 'https://files.example/get/setup.zip',
  finalUrl: 'https://files.example/get/setup.zip',
  referrer: 'https://files.example/downloads',
  filename: 'C:\\Users\\me\\Downloads\\setup.zip',
  mime: 'application/zip',
  totalBytes: 5_000_000,
  fileSize: 5_000_000,
  state: 'in_progress',
};

describe('Firefox bundle (browser.*, downloads.onCreated)', () => {
  it('registers its listeners at top level', async () => {
    const mock = createMockBrowser({ responses: baseResponses(), omit: ['downloads.onDeterminingFilename'] });
    loadFirefoxBundle(mock);
    for (const path of [
      'downloads.onCreated',
      'webRequest.onHeadersReceived',
      'webNavigation.onCommitted',
      'contextMenus.onClicked',
      'runtime.onMessage',
      'runtime.onInstalled',
      'tabs.onRemoved',
    ]) {
      assert.equal(mock.listeners[path]?.length, 1, path);
    }
    await settle();
    const ids = mock.callsTo('contextMenus.create').map(([p]) => p.id);
    assert.deepEqual(ids, ['spate-link', 'spate-selected-links', 'spate-all-links', 'spate-media']);
  });

  it('hands a matching download to Spate and removes it from the browser', async () => {
    const mock = createMockBrowser({ responses: baseResponses(), omit: ['downloads.onDeterminingFilename'] });
    loadFirefoxBundle(mock);
    mock.fire('downloads.onCreated', { ...zipItem, cookieStoreId: 'firefox-container-2' });
    await settle();
    assert.deepEqual(mock.callsTo('downloads.pause'), [[1]]);
    const [msg] = nativeMessages(mock);
    assert.deepEqual(msg, {
      type: 'download',
      interactive: true,
      items: [
        {
          url: 'https://files.example/get/setup.zip',
          fileName: 'setup.zip',
          referrer: 'https://files.example/downloads',
          cookies: 'sid=abc',
          userAgent: 'TestAgent/1.0',
          fileSize: 5_000_000,
          mime: 'application/zip',
          kind: 'http',
        },
      ],
    });
    const [query] = mock.callsTo('cookies.getAll')[0];
    assert.equal(query.storeId, 'firefox-container-2');
    assert.equal(query.firstPartyDomain, null);
    assert.deepEqual(mock.callsTo('downloads.cancel'), [[1]]);
    assert.deepEqual(mock.callsTo('downloads.erase'), [[{ id: 1 }]]);
    assert.equal(mock.callsTo('downloads.resume').length, 0);
  });

  it('resumes the browser download and flags an error when the host is missing', async () => {
    const mock = createMockBrowser({
      responses: baseResponses({ 'runtime.sendNativeMessage': new Error('No such native application com.spate.host') }),
      omit: ['downloads.onDeterminingFilename'],
    });
    loadFirefoxBundle(mock);
    mock.fire('downloads.onCreated', zipItem);
    await settle();
    assert.deepEqual(mock.callsTo('downloads.resume'), [[1]]);
    assert.equal(mock.callsTo('downloads.cancel').length, 0);
    assert.ok(mock.callsTo('action.setBadgeText').some(([d]) => d.text === '!' && d.tabId === undefined));
    const state = await message(mock, { cmd: 'popup-state', tabId: 7 });
    assert.match(state.lastError.message, /setup\.zip was left to the browser/);
    const ping = await message(mock, { cmd: 'ping' });
    assert.equal(ping.ok, false);
    assert.equal(ping.errorKind, 'not-installed');
  });

  it('leaves blob:, unlisted and excluded downloads alone', async () => {
    const mock = createMockBrowser({
      responses: baseResponses({ 'storage.local.get': { settings: { excludedSites: ['skip.example'] } } }),
      omit: ['downloads.onDeterminingFilename'],
    });
    loadFirefoxBundle(mock);
    mock.fire('downloads.onCreated', { ...zipItem, id: 2, url: 'blob:https://files.example/1', finalUrl: undefined });
    mock.fire('downloads.onCreated', { ...zipItem, id: 3, filename: '/tmp/page.html', mime: 'text/html', url: 'https://files.example/page' , finalUrl: undefined });
    mock.fire('downloads.onCreated', { ...zipItem, id: 4, referrer: 'https://www.skip.example/' });
    // A form submission whose response is a download.
    mock.fire('webRequest.onBeforeRequest', { tabId: 7, type: 'main_frame', method: 'POST', url: 'https://files.example/export.zip' });
    mock.fire('downloads.onCreated', { ...zipItem, id: 8, url: 'https://files.example/export.zip', finalUrl: 'https://files.example/export.zip' });
    await settle();
    assert.equal(mock.callsTo('downloads.pause').length, 0);
    assert.equal(nativeMessages(mock).length, 0);
  });

  it('detects HLS streams per tab and downloads them with kind "hls"', async () => {
    const mock = createMockBrowser({ responses: baseResponses(), omit: ['downloads.onDeterminingFilename'] });
    loadFirefoxBundle(mock);
    const response = (url, type, length) => ({
      tabId: 7,
      url,
      statusCode: 200,
      type: 'xmlhttprequest',
      responseHeaders: [
        { name: 'Content-Type', value: type },
        { name: 'Content-Length', value: String(length) },
      ],
    });
    mock.fire('webRequest.onHeadersReceived', response('https://cdn.example/v/master.m3u8', 'application/vnd.apple.mpegurl', 900));
    mock.fire('webRequest.onHeadersReceived', response('https://cdn.example/v/seg1.ts', 'video/mp2t', 2_000_000));
    mock.fire('webRequest.onHeadersReceived', response('https://cdn.example/v/master.m3u8', 'application/vnd.apple.mpegurl', 900));
    mock.fire('webRequest.onHeadersReceived', response('https://cdn.example/v/manifest.mpd', 'application/dash+xml', 4000));
    await settle();
    const badges = mock.callsTo('action.setBadgeText').filter(([d]) => d.tabId === 7).map(([d]) => d.text);
    assert.deepEqual(badges, ['1', '2']);

    const state = await message(mock, { cmd: 'popup-state', tabId: 7 });
    assert.deepEqual(state.media.map((m) => m.kind), ['hls', 'dash']);

    const reply = await message(mock, { cmd: 'download-media', tabId: 7, key: state.media[0].key });
    assert.equal(reply.ok, true);
    const [msg] = nativeMessages(mock);
    assert.equal(msg.items[0].kind, 'hls');
    assert.equal(msg.items[0].fileName, 'A video_ part 1.ts');
    assert.equal(msg.items[0].referrer, 'https://videos.example/watch?v=1');
    assert.equal(msg.items[0].fileSize, undefined);

    const dash = await message(mock, { cmd: 'download-media', tabId: 7, key: state.media[1].key });
    assert.equal(dash.ok, false);

    // A new page in the tab clears the list.
    mock.fire('webNavigation.onCommitted', { tabId: 7, frameId: 0, url: 'https://videos.example/next' });
    await settle();
    assert.equal((await message(mock, { cmd: 'popup-state', tabId: 7 })).media.length, 0);
  });

  it('sends all links of a page as one interactive batch', async () => {
    const links = [
      { url: 'https://files.example/a/one.zip', kind: 'link' },
      { url: 'https://mirror.example/two.iso', kind: 'link', fileName: 'two.iso' },
    ];
    const mock = createMockBrowser({
      responses: baseResponses({
        'scripting.executeScript': [{ frameId: 0, result: { pageUrl: 'https://files.example/a/', title: 'Files', links } }],
        'storage.local.get': { settings: { interactive: false } },
      }),
      omit: ['downloads.onDeterminingFilename'],
    });
    loadFirefoxBundle(mock);
    mock.fire('contextMenus.onClicked', { menuItemId: 'spate-all-links', frameId: 0, pageUrl: 'https://files.example/a/' }, { id: 7 });
    await settle();
    const [inject] = mock.callsTo('scripting.executeScript')[0];
    assert.deepEqual(inject.target, { tabId: 7, frameIds: [0] });
    assert.deepEqual(inject.args, [false]);
    const [msg] = nativeMessages(mock);
    assert.equal(msg.interactive, true);
    assert.deepEqual(
      msg.items.map((i) => [i.url, i.cookies, i.fileName, i.referrer]),
      [
        ['https://files.example/a/one.zip', 'sid=abc', undefined, 'https://files.example/a/'],
        ['https://mirror.example/two.iso', undefined, 'two.iso', 'https://files.example/a/'],
      ],
    );
  });
});

describe('Chromium modules (chrome.* callbacks, onDeterminingFilename)', () => {
  it('captures through onDeterminingFilename and always releases the download', async () => {
    const mock = createMockBrowser({ promises: false, responses: baseResponses() });
    globalThis.chrome = mock.api;
    try {
      await import(pathToFileURL(join(ROOT, 'src', 'background.js')).href);
      assert.equal(mock.listeners['downloads.onDeterminingFilename']?.length, 1);
      assert.equal(mock.listeners['downloads.onCreated'], undefined);
      await settle();
      const ids = mock.callsTo('contextMenus.create').map(([p]) => p.id);
      assert.ok(ids.includes('spate-downloads-page'));

      let suggested = 0;
      const [listener] = mock.listeners['downloads.onDeterminingFilename'];
      assert.equal(listener({ ...zipItem, filename: 'setup.zip', incognito: true }, () => (suggested += 1)), true);
      await settle();
      const [msg] = nativeMessages(mock);
      assert.equal(msg.items[0].fileName, 'setup.zip');
      assert.equal(mock.callsTo('cookies.getAll')[0][0].storeId, '1');
      assert.deepEqual(mock.callsTo('downloads.cancel'), [[1]]);
      assert.equal(suggested, 1);

      // Not captured: released immediately, nothing sent.
      assert.equal(listener({ ...zipItem, id: 5, url: 'data:text/plain,hi', finalUrl: '', filename: 'hi.txt' }, () => (suggested += 1)), true);
      await settle();
      assert.equal(suggested, 2);
      assert.equal(nativeMessages(mock).length, 1);

      // Host rejects: browser download resumes, still released once.
      mock.responses['runtime.sendNativeMessage'] = { ok: false, error: 'Spate is not running', version: '' };
      listener({ ...zipItem, id: 6 }, () => (suggested += 1));
      await settle();
      assert.equal(suggested, 3);
      assert.deepEqual(mock.callsTo('downloads.resume'), [[6]]);
    } finally {
      delete globalThis.chrome;
    }
  });
});
