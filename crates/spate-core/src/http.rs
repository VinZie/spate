//! HTTP client construction and response-header helpers.

use std::sync::Once;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Honest by default: bot protection on some CDNs drops TLS connections whose
/// User-Agent claims to be a browser while the TLS fingerprint is not.
/// Downloads captured from a browser carry that browser's User-Agent anyway.
pub const DEFAULT_USER_AGENT: &str = concat!("Spate/", env!("CARGO_PKG_VERSION"));

/// The Chrome-imitating default shipped before 0.1.0; treated as "unset" so
/// existing settings files pick up the new default.
pub const LEGACY_USER_AGENT_PREFIX: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36 Spate/";

/// `Retry-After` in its delta-seconds form (the HTTP-date form is ignored).
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    header_str(headers, reqwest::header::RETRY_AFTER)?.parse::<u64>().ok().map(Duration::from_secs)
}

/// Network settings shared by every connection of a download.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetConfig {
    pub user_agent: String,
    /// `http://`, `https://`, `socks5://` or `socks5h://` proxy URL. When
    /// unset the system proxy configuration is used.
    pub proxy: Option<String>,
    pub connect_timeout_secs: u64,
    /// A connection that delivers no data for this long is restarted.
    pub read_timeout_secs: u64,
    pub max_redirects: usize,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_USER_AGENT.to_string(),
            proxy: None,
            connect_timeout_secs: 15,
            read_timeout_secs: 30,
            max_redirects: 10,
        }
    }
}

static TLS_INIT: Once = Once::new();

/// Install the process-wide rustls crypto provider (ring).
pub fn init_tls() {
    TLS_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Build a client. Each download connection gets its own client so that every
/// connection is a separate TCP stream, even against HTTP/2 servers that
/// would otherwise multiplex them all onto one socket.
///
/// The client does not follow redirects; send requests through [`get`],
/// which follows them with the credentials scoped to every hop.
pub fn build_client(cfg: &NetConfig) -> Result<reqwest::Client> {
    init_tls();
    let ua = cfg.user_agent.trim();
    let ua = if ua.is_empty() || ua.starts_with(LEGACY_USER_AGENT_PREFIX) { DEFAULT_USER_AGENT } else { ua };
    let mut b = reqwest::Client::builder()
        .user_agent(ua.to_string())
        .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs.max(1)))
        .redirect(reqwest::redirect::Policy::none())
        .tcp_nodelay(true)
        .pool_max_idle_per_host(2)
        .http2_adaptive_window(true);
    if let Some(proxy) = cfg.proxy.as_deref().filter(|p| !p.trim().is_empty()) {
        b = b.proxy(reqwest::Proxy::all(proxy.trim()).map_err(|e| Error::Other(format!("invalid proxy: {e}")))?);
    }
    Ok(b.build()?)
}

/// GET `url` with `headers` (collected for `credentials_for`) plus `extra`,
/// following redirects. Each hop is sent the headers scoped to it, so cookies
/// and `Authorization` reach `credentials_for`'s origin only: not another
/// host, not a later hop there, not plain http after https. (reqwest's own
/// redirect handling restores the first request's headers on every hop and
/// compares only with the previous one, so A -> B -> B/x would hand A's
/// cookie to B.) `allow_hop` vets each redirect target before it is requested.
pub async fn get(
    client: &reqwest::Client,
    url: &reqwest::Url,
    credentials_for: &reqwest::Url,
    headers: &HeaderMap,
    extra: HeaderMap,
    max_redirects: usize,
    allow_hop: impl Fn(&reqwest::Url) -> Result<()>,
) -> Result<reqwest::Response> {
    use reqwest::StatusCode;

    let mut url = url.clone();
    let mut hops = 0;
    loop {
        let mut h = scoped_headers(headers, credentials_for, &url);
        for (name, value) in &extra {
            h.insert(name, value.clone());
        }
        let resp = client.get(url.clone()).headers(h).send().await?;
        let redirect = matches!(
            resp.status(),
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        );
        let Some(location) = header_str(resp.headers(), reqwest::header::LOCATION).filter(|_| redirect) else {
            return Ok(resp);
        };
        let next = url.join(location).map_err(|e| Error::InvalidUrl(format!("bad redirect target: {e}")))?;
        if !matches!(next.scheme(), "http" | "https") {
            return Err(Error::InvalidUrl(format!("redirect to unsupported scheme `{}`", next.scheme())));
        }
        hops += 1;
        if hops > max_redirects {
            return Err(Error::Other("too many redirects".into()));
        }
        allow_hop(&next)?;
        url = next;
    }
}

/// Turn user/browser supplied headers into a `HeaderMap`, dropping any that
/// are malformed, hop-by-hop, or managed by the engine. Conditional headers
/// copied from a browser that has a cached copy would turn the download
/// into `304 Not Modified` / `412 Precondition Failed`, so they go too.
pub fn header_map(headers: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        let lower = k.trim().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "range"
                | "if-range"
                | "if-none-match"
                | "if-modified-since"
                | "if-match"
                | "if-unmodified-since"
                | "host"
                | "content-length"
                | "transfer-encoding"
                | "accept-encoding"
                | "connection"
                | "keep-alive"
                | "proxy-connection"
                | "te"
                | "upgrade"
                | "expect"
        ) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(lower.as_bytes()), HeaderValue::from_str(v.trim())) {
            map.insert(name, value);
        }
    }
    // Byte ranges must refer to the stored representation.
    map.insert(reqwest::header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    map
}

/// Whether a request header carries the user's credentials for one site
/// (cookies, `Authorization`, API keys, session and CSRF tokens): such headers
/// stay on their origin and are never written to disk.
pub fn is_credential_header(name: &str) -> bool {
    const MARKERS: [&str; 10] = ["cookie", "auth", "token", "secret", "session", "password", "api-key", "apikey", "csrf", "xsrf"];
    let name = name.trim().to_ascii_lowercase();
    MARKERS.iter().any(|m| name.contains(m))
}

/// Whether credentials collected for `origin` may go with a request to
/// `target`: same scheme, host and port only, which also rules out sending
/// them over plain http after an https → http redirect.
pub fn same_origin(origin: &reqwest::Url, target: &reqwest::Url) -> bool {
    origin.scheme() == target.scheme()
        && origin.host_str() == target.host_str()
        && origin.port_or_known_default() == target.port_or_known_default()
}

/// The headers to send to `target`. Credentials (see
/// [`is_credential_header`]) were collected for `credentials_for` (the URL
/// the user or browser handed us), so they are dropped for any other origin,
/// such as a redirect target or a segment on a CDN.
pub fn scoped_headers(headers: &HeaderMap, credentials_for: &reqwest::Url, target: &reqwest::Url) -> HeaderMap {
    let mut scoped = headers.clone();
    if !same_origin(credentials_for, target) {
        let credentials: Vec<HeaderName> = scoped.keys().filter(|n| is_credential_header(n.as_str())).cloned().collect();
        for name in credentials {
            scoped.remove(name);
        }
    }
    scoped
}

/// Parse `bytes <first>-<last>/<total|*>`.
pub fn parse_content_range(v: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let (first, last) = range.trim().split_once('-')?;
    let first = first.trim().parse().ok()?;
    let last = last.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    (first <= last).then_some((first, last, total))
}

pub fn header_str(headers: &HeaderMap, name: impl reqwest::header::AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty())
}

/// Whether a failed request is worth retrying.
pub fn is_retryable_status(code: u16) -> bool {
    matches!(code, 408 | 425 | 429 | 500 | 502 | 503 | 504) || code >= 520
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range() {
        assert_eq!(parse_content_range("bytes 0-99/1234"), Some((0, 99, Some(1234))));
        assert_eq!(parse_content_range("bytes 100-199/*"), Some((100, 199, None)));
        assert_eq!(parse_content_range("bytes */1234"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
    }

    #[test]
    fn engine_headers_are_not_overridable() {
        let m = header_map(&[("Range".into(), "bytes=5-".into()), ("Cookie".into(), "a=b".into())]);
        assert!(m.get("range").is_none());
        assert_eq!(m.get("cookie").unwrap(), "a=b");
        assert_eq!(m.get("accept-encoding").unwrap(), "identity");
    }

    #[test]
    fn credentials_stay_with_their_origin() {
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        let h = header_map(&[
            ("Cookie".into(), "session=1".into()),
            ("Authorization".into(), "Bearer t".into()),
            ("X-CSRF-Token".into(), "c".into()),
            ("Referer".into(), "https://site.example/".into()),
        ]);
        let origin = url("https://site.example/file.zip");
        let same = scoped_headers(&h, &origin, &url("https://SITE.example:443/other?x"));
        assert_eq!(same.get("cookie").unwrap(), "session=1");
        assert_eq!(same.get("authorization").unwrap(), "Bearer t");
        for target in [
            "https://cdn.example/file.zip",       // another host
            "http://site.example/file.zip",       // https → http downgrade
            "https://site.example:8443/file.zip", // another port
            "https://evil.site.example/file.zip", // a subdomain is another origin
        ] {
            let scoped = scoped_headers(&h, &origin, &url(target));
            for credential in ["cookie", "authorization", "x-csrf-token"] {
                assert!(scoped.get(credential).is_none(), "{credential} sent to {target}");
            }
            assert_eq!(scoped.get("referer").unwrap(), "https://site.example/", "{target}");
        }
    }

    #[test]
    fn browser_cache_validators_are_dropped() {
        let m = header_map(&[
            ("If-None-Match".into(), "\"abc\"".into()),
            ("If-Modified-Since".into(), "Wed, 21 Oct 2026 07:28:00 GMT".into()),
            ("Connection".into(), "keep-alive".into()),
            ("Referer".into(), "https://example.com/".into()),
        ]);
        assert!(m.get("if-none-match").is_none());
        assert!(m.get("if-modified-since").is_none());
        assert!(m.get("connection").is_none());
        assert_eq!(m.get("referer").unwrap(), "https://example.com/");
    }
}
