//! URL helpers: clipboard detection, batch patterns, command-line parsing.

use spate_core::Category;

/// Whether `s` looks like a single absolute http(s) URL.
pub fn is_http_url(s: &str) -> bool {
    let s = s.trim();
    let Some((scheme, rest)) = s.split_once("://") else { return false };
    let scheme_ok = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    scheme_ok && !host.is_empty() && !s.chars().any(char::is_whitespace)
}

/// The URL in `text` if the clipboard holds exactly one http(s) URL.
pub fn single_url(text: &str) -> Option<String> {
    let t = text.trim();
    is_http_url(t).then(|| t.to_string())
}

/// Lower-case extension of the last path segment of `url` (query and
/// fragment ignored), if any.
pub fn url_extension(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path_start = rest.find('/')?;
    let path = &rest[path_start..];
    let path = path.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let (_, ext) = last.rsplit_once('.')?;
    (!ext.is_empty() && ext.len() <= 10 && ext.chars().all(|c| c.is_ascii_alphanumeric())).then(|| ext.to_ascii_lowercase())
}

/// Whether the URL points to a file type a download manager should offer
/// to fetch (any categorised extension, or an HLS playlist).
pub fn is_downloadable_ext(ext: &str) -> bool {
    ext == "m3u8"
        || Category::ALL
            .iter()
            .filter(|c| **c != Category::Other)
            .any(|c| c.extensions().contains(&ext))
}

/// Clipboard monitor rule: a single http(s) URL ending in a known
/// downloadable extension.
pub fn clipboard_download(text: &str) -> Option<String> {
    let url = single_url(text)?;
    let ext = url_extension(&url)?;
    is_downloadable_ext(&ext).then_some(url)
}

/// Best-effort file name from a URL (last path segment, percent-decoded).
pub fn file_name_from_url(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = rest.find('/').map(|i| &rest[i..]).unwrap_or("");
    let path = path.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let decoded = percent_decode(last);
    let cleaned: String = decoded
        .chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() { "download".into() } else { cleaned }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// All http(s) URLs in a block of text (one per line or separated by spaces).
pub fn extract_urls(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '<' || c == '>') {
        let t = token.trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | '[' | ']'));
        if is_http_url(t) && !out.iter().any(|u| u == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// Expand a batch pattern: every `*` in `pattern` is replaced by each value
/// of the range. Numeric ranges are zero-padded to `width` digits; letter
/// ranges (`a`..`z`) are used when both ends are single letters.
pub fn expand_pattern(pattern: &str, from: &str, to: &str, width: usize) -> Result<Vec<String>, String> {
    if !pattern.contains('*') {
        return Err("The address needs a * where the counter goes.".into());
    }
    const MAX: u64 = 10_000;
    let from = from.trim();
    let to = to.trim();
    let values: Vec<String> = if let (Ok(a), Ok(b)) = (from.parse::<u64>(), to.parse::<u64>()) {
        if a > b {
            return Err("The range start is after its end.".into());
        }
        if b - a >= MAX {
            return Err(format!("At most {MAX} addresses at once."));
        }
        (a..=b).map(|n| format!("{n:0width$}")).collect()
    } else {
        let (Some(a), Some(b)) = (single_letter(from), single_letter(to)) else {
            return Err("Use numbers (1 to 20) or single letters (a to z).".into());
        };
        if a.is_ascii_lowercase() != b.is_ascii_lowercase() || a > b {
            return Err("Letter ranges must go forward and share case.".into());
        }
        (a..=b).map(String::from).collect()
    };
    Ok(values.iter().map(|v| pattern.replace('*', v)).collect())
}

fn single_letter(s: &str) -> Option<char> {
    let mut it = s.chars();
    let c = it.next()?;
    (it.next().is_none() && c.is_ascii_alphabetic()).then_some(c)
}

/// Parsed command line of the desktop app.
#[derive(Debug, Default, PartialEq)]
pub struct Args {
    pub urls: Vec<String>,
    /// Start minimised / in the tray (used when launched by a helper).
    pub background: bool,
}

pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Args {
    let mut out = Args::default();
    for a in args {
        match a.as_str() {
            "--background" | "--minimized" | "--tray" | "-b" => out.background = true,
            s if is_http_url(s) => out.urls.push(s.to_string()),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_http_urls() {
        assert!(is_http_url("https://example.com/a.zip"));
        assert!(is_http_url("HTTP://EXAMPLE.COM"));
        assert!(!is_http_url("ftp://example.com/a.zip"));
        assert!(!is_http_url("https://"));
        assert!(!is_http_url("see https://example.com/a.zip"));
        assert_eq!(single_url("  https://x.org/f.iso \n").as_deref(), Some("https://x.org/f.iso"));
    }

    #[test]
    fn clipboard_rule() {
        assert_eq!(clipboard_download("https://x.org/pub/file.ZIP?token=1").as_deref(), Some("https://x.org/pub/file.ZIP?token=1"));
        assert!(clipboard_download("https://x.org/live/index.m3u8").is_some());
        assert!(clipboard_download("https://x.org/setup.exe#frag").is_some());
        assert!(clipboard_download("https://x.org/page.html").is_none());
        assert!(clipboard_download("https://x.org/").is_none());
        assert!(clipboard_download("https://x.org/dir.zip/").is_none());
        assert!(clipboard_download("https://x.org/a.zip https://x.org/b.zip").is_none());
        assert!(clipboard_download("just some text.zip").is_none());
    }

    #[test]
    fn extensions() {
        assert_eq!(url_extension("https://h/a/b.tar.gz").as_deref(), Some("gz"));
        assert_eq!(url_extension("https://h.com").as_deref(), None);
        assert_eq!(url_extension("https://h.com/x?y=a.zip").as_deref(), None);
        assert_eq!(url_extension("https://h.com/v1.2/readme").as_deref(), None);
    }

    #[test]
    fn names_from_urls() {
        assert_eq!(file_name_from_url("https://h/a/My%20File.zip?x=1"), "My File.zip");
        assert_eq!(file_name_from_url("https://h/"), "download");
        assert_eq!(file_name_from_url("https://h/a%2Fb%3F.txt"), "a_b_.txt");
        assert_eq!(file_name_from_url("https://h/100%"), "100%");
    }

    #[test]
    fn extracts_urls_from_text() {
        let text = "first https://a.com/1.zip\nhttps://a.com/2.zip, (https://a.com/3.zip)\nhttps://a.com/1.zip ftp://no";
        assert_eq!(extract_urls(text), vec!["https://a.com/1.zip", "https://a.com/2.zip", "https://a.com/3.zip"]);
    }

    #[test]
    fn batch_patterns() {
        assert_eq!(
            expand_pattern("https://h/img*.jpg", "1", "3", 2).unwrap(),
            vec!["https://h/img01.jpg", "https://h/img02.jpg", "https://h/img03.jpg"]
        );
        assert_eq!(expand_pattern("https://h/*/*.png", "a", "b", 0).unwrap(), vec!["https://h/a/a.png", "https://h/b/b.png"]);
        assert!(expand_pattern("https://h/x.jpg", "1", "2", 0).is_err());
        assert!(expand_pattern("https://h/*.jpg", "5", "2", 0).is_err());
        assert!(expand_pattern("https://h/*.jpg", "a", "Z", 0).is_err());
        assert!(expand_pattern("https://h/*.jpg", "0", "99999", 0).is_err());
    }

    #[test]
    fn command_line() {
        let a = parse_args(["--background".to_string(), "https://x/y.zip".into(), "chrome-extension://abc/".into()]);
        assert!(a.background);
        assert_eq!(a.urls, vec!["https://x/y.zip"]);
        assert_eq!(parse_args(Vec::<String>::new()), Args::default());
    }
}
