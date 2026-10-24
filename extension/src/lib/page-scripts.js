// Functions injected into web pages with scripting.executeScript({func}).
// They are serialized by the browser, so they must be self-contained: no
// references to anything outside their own body.

/**
 * Collect unique http(s) links and media sources of the current document
 * (or only those touching the selection). Returns {pageUrl, title, links};
 * `pageUrl` has no fragment.
 */
export function collectPageLinks(selectionOnly) {
  const MAX_LINKS = 2000;
  const links = [];
  const seen = new Set();
  const selection = selectionOnly ? window.getSelection() : null;
  const useSelection = Boolean(selection && selection.rangeCount && !selection.isCollapsed);
  const inScope = (el) => !useSelection || selection.containsNode(el, true);
  let pageHref = '';
  try {
    const page = new URL(location.href);
    page.hash = '';
    pageHref = page.href;
  } catch (e) {
    pageHref = '';
  }
  const add = (raw, kind, el) => {
    if (!raw || links.length >= MAX_LINKS) return;
    let u;
    try {
      u = new URL(raw, document.baseURI);
    } catch (e) {
      return;
    }
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return;
    u.hash = '';
    const href = u.href;
    if (href === pageHref || seen.has(href)) return;
    seen.add(href);
    const item = { url: href, kind };
    // Like browsers, honour `download` only on links to this page's own
    // origin (blob: and data: links are not collected at all).
    const name = el && el.getAttribute && u.origin === location.origin ? el.getAttribute('download') : null;
    if (name) item.fileName = name;
    links.push(item);
  };
  for (const a of document.querySelectorAll('a[href], area[href]')) {
    if (inScope(a)) add(a.href, 'link', a);
  }
  for (const el of document.querySelectorAll('video, audio, video source, audio source')) {
    if (!inScope(el)) continue;
    add(el.getAttribute('src') && el.src, 'media', null);
    if (el.currentSrc) add(el.currentSrc, 'media', null);
  }
  return { pageUrl: pageHref, title: document.title, links };
}
