import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import {
  DEFAULT_SETTINGS,
  basename,
  buildCookieHeader,
  classifyMedia,
  cleanItem,
  downloadKind,
  extensionMatches,
  fileExtension,
  fileNameFromUrl,
  formatSize,
  globToRegExp,
  headerMap,
  isExcluded,
  mediaFileName,
  mediaKey,
  mediaLabel,
  nativeErrorKind,
  normalizeSettings,
  parseExtensionList,
  parseSiteList,
  pickBestMedia,
  responseSize,
  sanitizeFileName,
  selectCookies,
  shortUrl,
  shouldCapture,
  siteMatches,
} from '../src/lib/core.js';

describe('wildcards', () => {
  it('globToRegExp handles * and ? and escapes the rest', () => {
    assert.ok(globToRegExp('*.example.com').test('cdn.example.com'));
    assert.ok(!globToRegExp('*.example.com').test('cdnXexampleYcom'));
    assert.ok(globToRegExp('r??').test('r01'));
    assert.ok(!globToRegExp('r??').test('r1'));
    assert.ok(globToRegExp('a+b(c)').test('A+B(C)'));
    assert.ok(!globToRegExp('a.b').test('axb'));
  });

  it('siteMatches covers bare domains, subdomains, wildcards and paths', () => {
    assert.ok(siteMatches('example.com', 'https://example.com/x.zip'));
    assert.ok(siteMatches('example.com', 'https://dl.example.com/x.zip'));
    assert.ok(!siteMatches('example.com', 'https://notexample.com/x.zip'));
    assert.ok(siteMatches('*.example.com', 'https://example.com/'));
    assert.ok(siteMatches('*.example.com', 'http://a.b.example.com/'));
    assert.ok(siteMatches('cdn?.example.*', 'https://cdn3.example.org/f'));
    assert.ok(!siteMatches('cdn?.example.*', 'https://cdn33.example.org/f'));
    assert.ok(siteMatches('https://example.com/downloads/', 'https://example.com/downloads/a.zip'));
    assert.ok(!siteMatches('example.com/downloads/', 'https://example.com/other/a.zip'));
    assert.ok(siteMatches('example.com/*.zip', 'https://example.com/a/b.zip?x=1'));
    assert.ok(siteMatches('example.com:8080', 'http://example.com:8080/a'));
    assert.ok(!siteMatches('', 'https://example.com/'));
    assert.ok(!siteMatches('example.com', 'ftp://example.com/'));
  });

  it('isExcluded checks every URL against every pattern', () => {
    assert.ok(isExcluded(['foo.org', 'example.com'], 'https://cdn.net/a.zip', 'https://www.example.com/page'));
    assert.ok(!isExcluded(['foo.org'], 'https://cdn.net/a.zip', undefined));
    assert.ok(!isExcluded([], 'https://cdn.net/a.zip'));
  });
});

describe('file types', () => {
  it('fileExtension works on names, paths and URLs', () => {
    assert.equal(fileExtension('setup.EXE'), 'exe');
    assert.equal(fileExtension('C:\\Users\\me\\Downloads\\a.tar.gz'), 'gz');
    assert.equal(fileExtension('https://x.org/dl/file.zip?token=1.2#frag'), 'zip');
    assert.equal(fileExtension('https://x.org/dl/'), '');
    assert.equal(fileExtension('.bashrc'), '');
    assert.equal(fileExtension('noext'), '');
    assert.equal(fileExtension('weird.ext with space'), '');
  });

  it('parseExtensionList normalizes and de-duplicates', () => {
    assert.deepEqual(parseExtensionList('ZIP, .rar;*.7z  zip\nr??'), ['zip', 'rar', '7z', 'r??']);
    assert.deepEqual(parseExtensionList(['mp4 mkv', 'bad/one']), ['mp4', 'mkv']);
  });

  it('extensionMatches supports wildcards', () => {
    assert.ok(extensionMatches('ZIP', ['zip']));
    assert.ok(extensionMatches('r07', ['r??']));
    assert.ok(extensionMatches('part12', ['part*']));
    assert.ok(!extensionMatches('rar5', ['r??']));
    assert.ok(!extensionMatches('', ['*']));
  });

  it('basename handles both separators', () => {
    assert.equal(basename('C:\\a\\b.zip'), 'b.zip');
    assert.equal(basename('/home/u/b.zip'), 'b.zip');
    assert.equal(basename(''), '');
  });
});

describe('settings', () => {
  it('normalizeSettings fills defaults and validates', () => {
    const s = normalizeSettings(undefined);
    assert.equal(s.enabled, true);
    assert.equal(s.interactive, true);
    assert.ok(s.extensions.includes('zip'));
    assert.deepEqual(s.excludedSites, []);
    const t = normalizeSettings({ enabled: 'yes', minSizeKB: -5, extensions: 'ISO', excludedSites: 'A.com\nb.org' });
    assert.equal(t.enabled, true);
    assert.equal(t.minSizeKB, 0);
    assert.deepEqual(t.extensions, ['iso']);
    assert.deepEqual(t.excludedSites, ['a.com', 'b.org']);
    assert.equal(normalizeSettings({ minSizeKB: '12.7' }).minSizeKB, 12);
    assert.deepEqual(parseSiteList('x.com x.com\ny.com'), ['x.com', 'y.com']);
  });
});

describe('shouldCapture', () => {
  const base = { url: 'https://example.com/files/setup.exe', fileName: 'setup.exe', mime: 'application/octet-stream', size: 10_000_000, state: 'in_progress' };
  const s = (over = {}) => normalizeSettings({ ...DEFAULT_SETTINGS, ...over });

  it('captures listed types', () => {
    assert.deepEqual(shouldCapture(base, s()), { capture: true, reason: 'ok' });
  });
  it('respects the on/off switch', () => {
    assert.equal(shouldCapture(base, s({ enabled: false })).reason, 'disabled');
  });
  it('skips non-http(s), blob: and data: URLs', () => {
    for (const url of ['blob:https://example.com/123', 'data:text/plain,hi', 'file:///C:/a.zip', 'ftp://x/a.zip']) {
      assert.equal(shouldCapture({ ...base, url }, s()).reason, 'unsupported-scheme', url);
    }
  });
  it('skips extension-initiated and finished downloads', () => {
    assert.equal(shouldCapture({ ...base, byExtensionId: 'abc' }, s()).reason, 'started-by-extension');
    assert.equal(shouldCapture({ ...base, state: 'complete' }, s()).reason, 'not-in-progress');
  });
  it('leaves responses to form submissions (POST) in the browser', () => {
    assert.equal(shouldCapture({ ...base, method: 'POST' }, s()).reason, 'not-a-get-request');
    assert.equal(shouldCapture({ ...base, method: 'get' }, s()).capture, true);
  });
  it('skips excluded sites (download URL or referrer)', () => {
    assert.equal(shouldCapture(base, s({ excludedSites: ['example.com'] })).reason, 'excluded-site');
    assert.equal(shouldCapture({ ...base, referrer: 'https://forum.test/t/1' }, s({ excludedSites: ['*.test'] })).reason, 'excluded-site');
  });
  it('applies the minimum size only when the size is known', () => {
    assert.equal(shouldCapture({ ...base, size: 1000 }, s({ minSizeKB: 100 })).reason, 'too-small');
    assert.equal(shouldCapture({ ...base, size: undefined }, s({ minSizeKB: 100 })).capture, true);
    assert.equal(shouldCapture({ ...base, size: -1 }, s({ minSizeKB: 100 })).capture, true);
  });
  it('filters by file type unless capturing everything', () => {
    const page = { ...base, url: 'https://example.com/report', fileName: 'report.html', mime: 'text/html' };
    assert.equal(shouldCapture(page, s()).reason, 'type-not-listed');
    assert.equal(shouldCapture(page, s({ captureAll: true })).capture, true);
  });
  it('falls back to the URL and then the MIME type for the file type', () => {
    assert.equal(shouldCapture({ ...base, fileName: '' }, s()).capture, true);
    assert.equal(shouldCapture({ ...base, fileName: '', url: 'https://example.com/get?id=1', mime: 'application/zip' }, s()).capture, true);
    assert.equal(shouldCapture({ ...base, fileName: '', url: 'https://example.com/get?id=1' }, s()).reason, 'type-not-listed');
  });
});

describe('media classification', () => {
  const cls = (url, contentType, size, statusCode = 200) => classifyMedia({ url, contentType, size, statusCode });

  it('detects HLS by MIME type or .m3u8 extension, regardless of size', () => {
    assert.equal(cls('https://cdn.x/live/index', 'application/vnd.apple.mpegURL; charset=utf-8', 300).kind, 'hls');
    assert.equal(cls('https://cdn.x/a/master.m3u8?tok=1', 'text/plain', 120).kind, 'hls');
    assert.equal(cls('https://cdn.x/a/master.m3u8', 'application/x-mpegurl').kind, 'hls');
  });
  it('detects DASH manifests', () => {
    assert.equal(cls('https://cdn.x/a/manifest.mpd', 'application/dash+xml', 2000).kind, 'dash');
    assert.equal(cls('https://cdn.x/a/manifest.mpd', 'application/octet-stream').kind, 'dash');
  });
  it('detects large video and audio files', () => {
    const v = cls('https://cdn.x/v/movie.mp4', 'video/mp4', 50_000_000);
    assert.deepEqual(v, { kind: 'video', mime: 'video/mp4', ext: 'mp4', size: 50_000_000 });
    assert.equal(cls('https://cdn.x/a/song.mp3', 'audio/mpeg', 4_000_000).kind, 'audio');
    assert.equal(cls('https://cdn.x/v/clip.webm', 'application/octet-stream', 2_000_000).kind, 'video');
    assert.equal(cls('https://cdn.x/v/stream', 'video/webm', undefined).kind, 'video');
  });
  it('ignores small fragments and segment files', () => {
    assert.equal(cls('https://cdn.x/v/movie.mp4', 'video/mp4', 100_000), null);
    assert.equal(cls('https://cdn.x/hls/seg-001.ts', 'video/mp2t', 2_000_000), null);
    assert.equal(cls('https://cdn.x/hls/seg-001.ts', 'application/octet-stream', 2_000_000), null);
    assert.equal(cls('https://cdn.x/dash/chunk-5.m4s', 'video/iso.segment', 2_000_000), null);
    assert.equal(cls('https://cdn.x/dash/chunk-5.m4s', 'video/mp4', 2_000_000), null);
  });
  it('ignores non-media and failed responses', () => {
    assert.equal(cls('https://x/page.mp4', 'text/html', 9_000_000), null);
    assert.equal(cls('https://x/master.m3u8', 'text/html', 100), null);
    assert.equal(cls('https://x/img.png', 'image/png', 9_000_000), null);
    assert.equal(cls('https://x/movie.mp4', 'video/mp4', 9_000_000, 404), null);
    assert.equal(cls('blob:https://x/1', 'video/mp4', 9_000_000), null);
  });
  it('reads the full size from Content-Range for partial responses', () => {
    const h = headerMap([{ name: 'Content-Length', value: '1048576' }, { name: 'Content-Range', value: 'bytes 0-1048575/734003200' }]);
    assert.equal(responseSize(h), 734003200);
    assert.equal(responseSize(headerMap([{ name: 'content-length', value: '42' }])), 42);
    assert.equal(responseSize(headerMap([{ name: 'Content-Range', value: 'bytes 0-1/*' }])), undefined);
  });
  it('de-duplicates byte-range variants of one resource', () => {
    assert.equal(
      mediaKey('https://v.x/videoplayback?id=1&range=0-1000#t=3'),
      mediaKey('https://v.x/videoplayback?id=1&range=1001-2000'),
    );
    assert.notEqual(mediaKey('https://v.x/a.mp4?id=1'), mediaKey('https://v.x/a.mp4?id=2'));
  });
  it('labels and picks media sensibly', () => {
    assert.equal(mediaLabel({ kind: 'hls' }), 'HLS');
    assert.equal(mediaLabel({ kind: 'video', ext: 'mp4' }), 'MP4');
    assert.equal(mediaLabel({ kind: 'audio', mime: 'audio/mpeg' }), 'MP3');
    const list = [
      { kind: 'dash', url: 'd', detectedAt: 1 },
      { kind: 'video', url: 'small', size: 1_000_000, detectedAt: 2 },
      { kind: 'video', url: 'big', size: 9_000_000, detectedAt: 3 },
    ];
    assert.equal(pickBestMedia(list).url, 'big');
    assert.equal(pickBestMedia([...list, { kind: 'hls', url: 'variant', detectedAt: 9 }, { kind: 'hls', url: 'master', detectedAt: 4 }]).url, 'master');
    assert.equal(pickBestMedia([{ kind: 'dash', url: 'd' }]), null);
  });
  it('names media files from the URL or the page title', () => {
    assert.equal(mediaFileName({ kind: 'hls', url: 'https://x/master.m3u8' }, 'My: Video?'), 'My_ Video_.ts');
    assert.equal(mediaFileName({ kind: 'video', url: 'https://x/v/clip%20one.mp4', ext: 'mp4' }, 'Page'), 'clip one.mp4');
    assert.equal(mediaFileName({ kind: 'video', url: 'https://x/videoplayback', mime: 'video/webm' }, 'Page'), 'Page.webm');
  });
  it('downloadKind marks HLS playlists', () => {
    assert.equal(downloadKind('https://x/a.m3u8', '', ''), 'hls');
    assert.equal(downloadKind('https://x/get', 'application/vnd.apple.mpegurl', ''), 'hls');
    assert.equal(downloadKind('https://x/a.zip', 'application/zip', 'a.zip'), 'http');
  });
});

describe('cookies', () => {
  it('builds a Cookie header with longest paths first and no duplicates', () => {
    const header = buildCookieHeader([
      { name: 'sid', value: 'root', path: '/', domain: '.x.com' },
      { name: 'sid', value: 'deep', path: '/app/dl', domain: 'x.com' },
      { name: 'pref', value: 'a=b', path: '/app', domain: 'x.com' },
      { name: 'sid', value: 'root', path: '/', domain: '.x.com' },
      { name: '', value: 'flag', path: '/', domain: 'x.com' },
    ]);
    assert.equal(header, 'sid=deep; pref=a=b; sid=root; flag');
    assert.equal(buildCookieHeader([]), '');
    assert.equal(buildCookieHeader(undefined), '');
  });
  it('keeps partitioned cookies only for the matching top-level site', () => {
    const cookies = [
      { name: 'a', value: '1' },
      { name: 'b', value: '2', partitionKey: { topLevelSite: 'https://shop.com' } },
      { name: 'c', value: '3', partitionKey: { topLevelSite: 'https://other.com' } },
      { name: 'd', value: '4', partitionKey: {} },
    ];
    assert.deepEqual(selectCookies(cookies, 'www.shop.com').map((c) => c.name), ['a', 'b', 'd']);
    assert.deepEqual(selectCookies(cookies, '').map((c) => c.name), ['a', 'd']);
  });
});

describe('items and formatting', () => {
  it('cleanItem omits unknown fields', () => {
    assert.deepEqual(
      cleanItem({ url: 'https://x/a', fileName: '', referrer: null, cookies: undefined, fileSize: -1, mime: 'a/b', kind: 'http' }),
      { url: 'https://x/a', mime: 'a/b', kind: 'http' },
    );
    assert.equal(cleanItem({ url: 'u', fileSize: 12.9 }).fileSize, 12);
  });
  it('sanitizes file names', () => {
    assert.equal(sanitizeFileName('a/b\\c:d*e?f"g<h>i|j'), 'a_b_c_d_e_f_g_h_i_j');
    assert.equal(sanitizeFileName('  ..name.  '), 'name');
    const long = sanitizeFileName(`${'x'.repeat(300)}.zip`);
    assert.equal(long.length, 180);
    assert.ok(long.endsWith('.zip'));
    assert.equal(fileNameFromUrl('https://x/dl/My%20File.zip?x=1'), 'My File.zip');
    assert.equal(fileNameFromUrl('https://x/'), '');
  });
  it('formats sizes and URLs', () => {
    assert.equal(formatSize(undefined), '');
    assert.equal(formatSize(512), '512 B');
    assert.equal(formatSize(1536), '1.5 KB');
    assert.equal(formatSize(734003200), '700 MB');
    assert.equal(shortUrl('https://cdn.example.com/a.mp4'), 'cdn.example.com/a.mp4');
    const s = shortUrl(`https://cdn.example.com/${'d/'.repeat(40)}video.mp4`, 40);
    assert.ok(s.length <= 44 && s.endsWith('video.mp4'), s);
  });
  it('classifies native messaging errors', () => {
    assert.equal(nativeErrorKind('Specified native messaging host not found.'), 'not-installed');
    assert.equal(nativeErrorKind('No such native application com.spate.host'), 'not-installed');
    assert.equal(nativeErrorKind('Access to the specified native messaging host is forbidden.'), 'forbidden');
    assert.equal(nativeErrorKind('Spate is not running'), 'app-not-running');
    assert.equal(nativeErrorKind('Native host has exited.'), 'host-error');
    assert.equal(nativeErrorKind('Timed out waiting for the Spate native host'), 'timeout');
    assert.equal(nativeErrorKind('disk full'), 'other');
  });
});
