import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { after, describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';
import { inflateRawSync } from 'node:zlib';
import { build, createZip, manifestFiles } from '../build.mjs';
import { crc32, ICON_FILES } from '../scripts/make-icons.mjs';

const ROOT = join(fileURLToPath(import.meta.url), '..', '..');
const readJson = (p) => JSON.parse(readFileSync(p, 'utf8'));
const chrome = readJson(join(ROOT, 'manifest.json'));
const firefox = readJson(join(ROOT, 'manifest.firefox.json'));
const ids = readJson(join(ROOT, 'extension-id.json'));

/** Read every entry of a ZIP file, verifying sizes and CRCs. */
function readZip(buf) {
  const eocd = buf.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]));
  assert.ok(eocd >= 0, 'end of central directory');
  const count = buf.readUInt16LE(eocd + 10);
  let p = buf.readUInt32LE(eocd + 16);
  const out = new Map();
  for (let i = 0; i < count; i += 1) {
    assert.equal(buf.readUInt32LE(p), 0x02014b50);
    const method = buf.readUInt16LE(p + 10);
    const crc = buf.readUInt32LE(p + 16);
    const compSize = buf.readUInt32LE(p + 20);
    const size = buf.readUInt32LE(p + 24);
    const nameLen = buf.readUInt16LE(p + 28);
    const local = buf.readUInt32LE(p + 42);
    const name = buf.toString('utf8', p + 46, p + 46 + nameLen);
    assert.equal(buf.readUInt32LE(local), 0x04034b50);
    const start = local + 30 + buf.readUInt16LE(local + 26) + buf.readUInt16LE(local + 28);
    const raw = buf.subarray(start, start + compSize);
    const data = method === 8 ? inflateRawSync(raw) : Buffer.from(raw);
    assert.equal(data.length, size, name);
    assert.equal(crc32(data), crc, name);
    out.set(name, data);
    p += 46 + nameLen + buf.readUInt16LE(p + 30) + buf.readUInt16LE(p + 32);
  }
  return out;
}

describe('manifests', () => {
  it('pins the Chromium extension ID through the public key', () => {
    assert.equal(chrome.key, ids.key);
    assert.equal(ids.id, 'lliejfalegflnfhnefcinabnpbpbbble');
  });

  it('declares the Firefox add-on ID and an event-page background', () => {
    assert.equal(firefox.browser_specific_settings.gecko.id, 'spate@vinzie.github.io');
    assert.deepEqual(firefox.background, { scripts: ['src/background.bundle.js'] });
    assert.equal(firefox.key, undefined);
  });

  it('keeps both manifests in step', () => {
    assert.equal(chrome.manifest_version, 3);
    assert.equal(firefox.manifest_version, 3);
    assert.equal(firefox.version, chrome.version);
    assert.deepEqual([...firefox.permissions].sort(), [...chrome.permissions].sort());
    assert.deepEqual(firefox.host_permissions, chrome.host_permissions);
    assert.deepEqual(firefox.action, chrome.action);
    assert.deepEqual(firefox.options_ui, chrome.options_ui);
  });

  it('refers only to files that exist (Chromium loads this folder unpacked)', () => {
    const missing = manifestFiles(chrome).filter((p) => !existsSync(join(ROOT, p)));
    assert.deepEqual(missing, []);
    assert.equal(chrome.background.type, 'module');
  });

  it('asks for no permission it does not use', () => {
    const source = readdirSync(join(ROOT, 'src', 'lib'))
      .map((f) => readFileSync(join(ROOT, 'src', 'lib', f), 'utf8'))
      .concat(readFileSync(join(ROOT, 'src', 'background.js'), 'utf8'))
      .join('\n');
    for (const perm of chrome.permissions) {
      assert.ok(source.includes(`api.${perm}`) || (perm === 'nativeMessaging' && source.includes('sendNativeMessage')), perm);
    }
  });
});

describe('icons', () => {
  it('are PNG files of the size their name says', () => {
    for (const [name, size] of ICON_FILES) {
      const png = readFileSync(join(ROOT, 'icons', name));
      assert.deepEqual([...png.subarray(0, 8)], [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], name);
      assert.equal(png.toString('ascii', 12, 16), 'IHDR');
      assert.equal(png.readUInt32BE(16), size, name);
      assert.equal(png.readUInt32BE(20), size, name);
    }
  });
});

describe('build', () => {
  const out = mkdtempSync(join(tmpdir(), 'spate-ext-'));
  after(() => rmSync(out, { recursive: true, force: true }));

  it('produces loadable Chromium and Firefox trees and valid zips', () => {
    const result = build({ outDir: out, log: () => {} });
    const chromeZip = readZip(readFileSync(result.chromeZip));
    const firefoxZip = readZip(readFileSync(result.firefoxZip));

    const zChrome = JSON.parse(chromeZip.get('manifest.json'));
    assert.equal(zChrome.key, ids.key);
    for (const p of manifestFiles(zChrome)) assert.ok(chromeZip.has(p), `chromium zip: ${p}`);

    const zFirefox = JSON.parse(firefoxZip.get('manifest.json'));
    assert.equal(zFirefox.browser_specific_settings.gecko.id, 'spate@vinzie.github.io');
    for (const p of manifestFiles(zFirefox)) assert.ok(firefoxZip.has(p), `firefox zip: ${p}`);
    assert.ok(!firefoxZip.has('src/background.js'));
    const bundle = firefoxZip.get('src/background.bundle.js').toString('utf8');
    assert.ok(!/^\s*(import|export)\s/m.test(bundle), 'bundle has no module syntax');
    for (const name of [...chromeZip.keys(), ...firefoxZip.keys()]) {
      assert.ok(!/^(test|dist|scripts)\/|\.mjs$|^package\.json$/.test(name), `unexpected file ${name}`);
    }
  });

  it('writes zip files other tools can read back', () => {
    const entries = [
      { name: 'a.txt', data: Buffer.from('hello hello hello hello') },
      { name: 'dir/b.bin', data: Buffer.from([1, 2, 3]) },
    ];
    const zip = readZip(createZip(entries));
    assert.equal(zip.get('a.txt').toString(), 'hello hello hello hello');
    assert.deepEqual([...zip.get('dir/b.bin')], [1, 2, 3]);
  });
});
