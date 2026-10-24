//! File-name discovery and sanitising.

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

/// Extract the file name from a `Content-Disposition` header value,
/// preferring the RFC 5987 `filename*` form over plain `filename`.
pub fn from_content_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for param in split_params(value).into_iter().skip(1) {
        let Some((key, val)) = param.split_once('=') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim();
        match key.as_str() {
            "filename*" => extended = decode_ext_value(val),
            "filename" => plain = Some(unquote(val)),
            _ => {}
        }
    }
    extended.or(plain).map(|n| basename(&n)).filter(|n| !n.is_empty())
}

/// Last path segment of a URL, percent-decoded.
pub fn from_url(url: &url::Url) -> Option<String> {
    let seg = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    let decoded = percent_decode_str(seg).decode_utf8_lossy().into_owned();
    Some(basename(&decoded)).filter(|n| !n.is_empty())
}

/// Best-effort extension for responses whose URL and headers carry no name.
pub fn extension_for_mime(mime: &str) -> Option<&'static str> {
    let mime = mime.split(';').next()?.trim().to_ascii_lowercase();
    Some(match mime.as_str() {
        "application/pdf" => "pdf",
        "application/zip" | "application/x-zip-compressed" => "zip",
        "application/x-7z-compressed" => "7z",
        "application/vnd.rar" | "application/x-rar-compressed" => "rar",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-msdownload" | "application/x-msdos-program" => "exe",
        "application/x-msi" => "msi",
        "application/x-iso9660-image" => "iso",
        "application/json" => "json",
        "text/html" => "html",
        "text/plain" => "txt",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/ogg" => "ogg",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/mp2t" => "ts",
        _ => return None,
    })
}

/// Make a name safe for every mainstream filesystem (Windows being the
/// strictest): no separators or reserved characters, no reserved device
/// names, no trailing dots/spaces, bounded length.
pub fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = out.trim_end_matches(['.', ' ']).trim_start().to_string();
    out = if trimmed.is_empty() { "download".to_string() } else { trimmed };

    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        out.insert(0, '_');
    }

    const MAX_BYTES: usize = 200;
    if out.len() > MAX_BYTES {
        let (stem, ext) = split_ext(&out);
        let keep = MAX_BYTES.saturating_sub(ext.len() + 1);
        let mut cut = keep.min(stem.len());
        while !stem.is_char_boundary(cut) {
            cut -= 1;
        }
        out = if ext.is_empty() { stem[..cut].to_string() } else { format!("{}.{ext}", &stem[..cut]) };
    }
    out
}

/// `dir/name`, or `dir/name (2).ext` etc. if that (or its partial-download
/// sibling) already exists.
pub fn unique_path(dir: &Path, name: &str, partial_suffix: &str) -> PathBuf {
    let taken = |p: &Path| p.exists() || with_suffix(p, partial_suffix).exists();
    let first = dir.join(name);
    if !taken(&first) {
        return first;
    }
    let (stem, ext) = split_ext(name);
    (2..)
        .map(|i| {
            let n = if ext.is_empty() { format!("{stem} ({i})") } else { format!("{stem} ({i}).{ext}") };
            dir.join(n)
        })
        .find(|p| !taken(p))
        .expect("unbounded search")
}

pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

fn basename(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    sanitize(name.trim())
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else {
                out.push(c);
            }
        }
        out
    } else {
        // Some servers send raw percent-encoded names in the plain form.
        percent_decode_str(v).decode_utf8_lossy().into_owned()
    }
}

/// RFC 5987 ext-value: `charset'lang'pct-encoded`.
fn decode_ext_value(v: &str) -> Option<String> {
    let v = unquote(v);
    let mut parts = v.splitn(3, '\'');
    let charset = parts.next()?.to_ascii_lowercase();
    let _lang = parts.next()?;
    let encoded = parts.next()?;
    let bytes: Vec<u8> = percent_decode_str(encoded).collect();
    Some(match charset.as_str() {
        "utf-8" | "utf8" => String::from_utf8_lossy(&bytes).into_owned(),
        // ISO-8859-1 maps bytes 1:1 onto the first 256 code points.
        _ => bytes.iter().map(|&b| b as char).collect(),
    })
}

fn split_params(v: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_quotes = false;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in v.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => {
                out.push(&v[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&v[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_forms() {
        assert_eq!(from_content_disposition("attachment; filename=\"a b.zip\"").unwrap(), "a b.zip");
        assert_eq!(from_content_disposition("attachment; filename=plain.iso").unwrap(), "plain.iso");
        assert_eq!(
            from_content_disposition("attachment; filename=\"fallback.pdf\"; filename*=UTF-8''%E2%82%AC%20rates.pdf")
                .unwrap(),
            "€ rates.pdf"
        );
        assert_eq!(from_content_disposition("inline; filename=\"semi;colon.txt\"").unwrap(), "semi;colon.txt");
        assert_eq!(from_content_disposition("attachment; filename=\"../../evil.exe\"").unwrap(), "evil.exe");
        assert_eq!(from_content_disposition("attachment; filename=C:\\Windows\\evil.exe").unwrap(), "evil.exe");
        assert_eq!(from_content_disposition("attachment"), None);
    }

    #[test]
    fn url_names() {
        let u = url::Url::parse("https://example.com/files/My%20Setup.exe?x=1").unwrap();
        assert_eq!(from_url(&u).unwrap(), "My Setup.exe");
        let u = url::Url::parse("https://example.com/dir/").unwrap();
        assert_eq!(from_url(&u).unwrap(), "dir");
        let u = url::Url::parse("https://example.com/").unwrap();
        assert_eq!(from_url(&u), None);
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize("a<b>c:d.txt"), "a_b_c_d.txt");
        assert_eq!(sanitize("con.txt"), "_con.txt");
        assert_eq!(sanitize("name. . "), "name");
        assert_eq!(sanitize(""), "download");
        let long = format!("{}.mp4", "x".repeat(500));
        let s = sanitize(&long);
        assert!(s.len() <= 200 && s.ends_with(".mp4"));
    }
}
