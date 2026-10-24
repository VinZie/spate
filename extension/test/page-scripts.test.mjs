// The injected page functions, run the way scripting.executeScript runs them:
// serialized and evaluated in another context (here a vm context with a
// minimal fake DOM), which also proves they are self-contained.

import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import vm from 'node:vm';
import { collectPageLinks } from '../src/lib/page-scripts.js';
import { toPlain } from './mock-browser.mjs';

function runInPage(func, args, { url, anchors = [] }) {
  const elements = anchors.map(({ href, download }) => ({
    href: new URL(href, url).href,
    getAttribute: (name) => (name === 'download' ? (download ?? null) : null),
  }));
  const context = vm.createContext({
    URL,
    location: new URL(url),
    window: {},
    document: {
      baseURI: url,
      title: 'Files',
      querySelectorAll: (selector) => (selector.startsWith('a[href]') ? elements : []),
    },
  });
  return toPlain(vm.runInContext(`(${func})(...${JSON.stringify(args)})`, context));
}

describe('collectPageLinks', () => {
  it('honours download names only for links to the page\'s own origin', () => {
    const result = runInPage(collectPageLinks, [false], {
      url: 'https://files.example/a/#latest',
      anchors: [
        { href: 'report?id=1', download: 'Report.pdf' },
        { href: 'https://mirror.example/two.iso', download: 'evil.lnk' },
        { href: 'http://files.example/a/plain.zip', download: 'other-scheme.zip' },
        { href: '/a/three.zip' },
        { href: 'blob:https://files.example/1', download: 'clip.webm' },
        { href: 'https://files.example/a/#other' },
      ],
    });
    assert.equal(result.pageUrl, 'https://files.example/a/'); // no fragment
    assert.deepEqual(result.links, [
      { url: 'https://files.example/a/report?id=1', kind: 'link', fileName: 'Report.pdf' },
      { url: 'https://mirror.example/two.iso', kind: 'link' },
      { url: 'http://files.example/a/plain.zip', kind: 'link' },
      { url: 'https://files.example/a/three.zip', kind: 'link' },
    ]);
  });
});
