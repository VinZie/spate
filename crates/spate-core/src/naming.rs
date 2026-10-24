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

/// Longest name [`sanitize`] returns, in bytes (well under the 255 UTF-16
/// units NTFS allows, leaving room for ` (2)` and `.part`).
const MAX_NAME_BYTES: usize = 200;
/// Longest extension kept whole when a name has to be shortened.
const MAX_EXT_BYTES: usize = 32;

/// Make a name safe for every mainstream filesystem (Windows being the
/// strictest): no separators or reserved characters, no reserved device
/// names, no trailing dots/spaces, bounded length.
///
/// It also makes the name read the way it will be saved: bidi controls and
/// other invisible characters are removed (they can show `exe.pdf` for a
/// file that ends in `.exe`), and runs of spaces become one space so that
/// padding cannot push the real extension out of view.
pub fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        let c = match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c if is_invisible(c) => continue,
            c if c.is_whitespace() || is_blank(c) => ' ',
            c => c,
        };
        if c == ' ' && out.ends_with(' ') {
            continue;
        }
        out.push(c);
    }
    out = trim_name(&out);
    if is_reserved(&out) {
        out.insert(0, '_');
    }

    if out.len() > MAX_NAME_BYTES {
        let (stem, ext) = split_ext(&out);
        out = if !ext.is_empty() && ext.len() <= MAX_EXT_BYTES {
            format!("{}.{ext}", truncate_bytes(stem, MAX_NAME_BYTES - ext.len() - 1))
        } else {
            // An extension cut short could turn into another one (`.exe`
            // from `.exe_and_more`), so a name shortened this way keeps none.
            truncate_bytes(&out.replace('.', "_"), MAX_NAME_BYTES).to_string()
        };
        out = trim_name(&out);
    }
    out
}

/// Without leading spaces and trailing dots/spaces (Windows drops those),
/// or `download` if nothing is left.
fn trim_name(name: &str) -> String {
    let trimmed = name.trim_end_matches(['.', ' ']).trim_start_matches(' ');
    if trimmed.is_empty() { "download".to_string() } else { trimmed.to_string() }
}

fn truncate_bytes(s: &str, max: usize) -> &str {
    let mut cut = max.min(s.len());
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// Format characters that render as nothing or reorder the text around
/// them: bidi controls, zero-width characters, joiners, the BOM, soft
/// hyphens, invisible math operators and tag characters.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'
            | '\u{1160}'
            | '\u{17B4}'
            | '\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// Characters that are not whitespace but draw as an empty space.
fn is_blank(c: char) -> bool {
    matches!(c, '\u{2800}' | '\u{3164}' | '\u{FFA0}')
}

/// Whether Windows treats `name` as a device (`CON`, `COM1`, `LPT²`...),
/// whatever extension follows and with trailing spaces ignored.
fn is_reserved(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6",
        "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6",
        "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
    ];
    let device = name.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    RESERVED.contains(&device.as_str())
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
        assert_eq!(sanitize("name. . "), "name");
        assert_eq!(sanitize(""), "download");
        assert_eq!(sanitize(" \u{3000} "), "download");
        assert_eq!(sanitize("tab\there.txt"), "tab_here.txt");
        let long = format!("{}.mp4", "x".repeat(500));
        let s = sanitize(&long);
        assert!(s.len() <= 200 && s.ends_with(".mp4"));
    }

    #[test]
    fn reserved_device_names() {
        for name in [
            "con.txt", "CON", "con .txt", "CON   .txt", "nul.tar.gz", "Aux.", "COM0.log", "com9", "COM¹.txt", "com².txt",
            "COM³", "LPT¹.txt", "lpt².txt", "LPT³", "LPT0", "conin$", "CONIN$.txt", "CONOUT$", "conout$ .log",
        ] {
            let s = sanitize(name);
            assert!(s.starts_with('_'), "{name:?} -> {s:?}");
        }
        assert_eq!(sanitize("CON .txt"), "_CON .txt");
        for name in ["console.txt", "COM10.txt", "LPT.txt", "conin.txt", "nul_.txt", "my con.txt"] {
            assert_eq!(sanitize(name), name);
        }
    }

    #[test]
    fn invisible_and_bidi_characters_are_removed() {
        // Displayed as "invoiceexe.doc" while the U+202E is in it.
        assert_eq!(sanitize("invoice\u{202E}cod.exe"), "invoicecod.exe");
        for c in ['\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}'] {
            assert_eq!(sanitize(&format!("a{c}b.pdf")), "ab.pdf", "U+{:04X}", c as u32);
        }
        for c in ['\u{200E}', '\u{200F}', '\u{061C}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{2060}', '\u{FEFF}', '\u{00AD}'] {
            assert_eq!(sanitize(&format!("a{c}b.pdf")), "ab.pdf", "U+{:04X}", c as u32);
        }
        assert_eq!(sanitize("\u{FEFF}report.pdf\u{200B}"), "report.pdf");
        assert_eq!(sanitize("€ rates.pdf"), "€ rates.pdf");
    }

    #[test]
    fn whitespace_runs_collapse() {
        let padded = format!("invoice.pdf{}.exe", " ".repeat(150));
        assert_eq!(sanitize(&padded), "invoice.pdf .exe");
        assert_eq!(sanitize("a \u{00A0}\u{2003}\u{3000}\u{2800}\u{3164} b.txt"), "a b.txt");
        assert_eq!(sanitize("  lead and   trail  .txt  "), "lead and trail .txt");
    }

    #[test]
    fn length_cap_holds() {
        let cases = [
            format!("a.{}", "x".repeat(500)),
            format!(".{}", "x".repeat(500)),
            format!("{}.{}", "y".repeat(300), "x".repeat(300)),
            format!("{}.mp4", "é".repeat(300)),
            format!("{}.mp4", "x".repeat(199)),
            format!("CON.{}", "x".repeat(198)),
            "a.".repeat(300),
            format!("{} .{}", "x".repeat(198), "y".repeat(40)),
            format!("{}\u{202E}.txt", "x".repeat(400)),
        ];
        for name in &cases {
            let s = sanitize(name);
            assert!(s.len() <= 200, "{} bytes from {} bytes", s.len(), name.len());
            assert!(!s.ends_with(['.', ' ']) && !s.is_empty(), "{s:?}");
            assert_eq!(sanitize(&s), s, "sanitizing is idempotent");
        }
        // A long extension is not cut down into a different one.
        let tricky = format!("{}.exe.{}", "a".repeat(196), "b".repeat(100));
        let s = sanitize(&tricky);
        assert!(s.len() <= 200 && split_ext(&s).1.is_empty(), "{s:?}");
        let tricky = format!("{}.exe{}", "a".repeat(196), "b".repeat(100));
        assert!(!sanitize(&tricky).to_ascii_lowercase().ends_with(".exe"));
        // A short one is always kept.
        let s = sanitize(&format!("{}.tar.gz", "n".repeat(400)));
        assert!(s.len() == 200 && s.ends_with(".gz"), "{s}");
    }
}
