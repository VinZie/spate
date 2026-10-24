//! Argument parsing helpers shared by the subcommands, plus translation of
//! the classic download-manager command-line switches (`/d URL /p dir ...`).

use std::ffi::OsString;

/// Parse a byte count such as `500K`, `2M`, `1.5G`, `750KiB/s` or `1048576`.
/// Suffixes are binary (K = 1024), as download managers conventionally use.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let lower = s.trim().to_ascii_lowercase();
    let lower = lower.strip_suffix("/s").unwrap_or(&lower).trim_end();
    let split = lower.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(lower.len());
    let (num, unit) = lower.split_at(split);
    let mult: u64 = match unit.trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        other => return Err(format!("unknown size unit `{other}` (use K, M, G or T)")),
    };
    if num.is_empty() {
        return Err(format!("`{s}` is not a size (examples: 500K, 2M, 1048576)"));
    }
    if !num.contains('.') {
        let n: u64 = num.parse().map_err(|_| format!("`{s}` is not a valid size"))?;
        return n.checked_mul(mult).ok_or_else(|| format!("`{s}` is too large"));
    }
    let n: f64 = num.parse().map_err(|_| format!("`{s}` is not a valid size"))?;
    let v = n * mult as f64;
    if !v.is_finite() || v >= u64::MAX as f64 {
        return Err(format!("`{s}` is too large"));
    }
    Ok(v.round() as u64)
}

/// Parse `Name: value` (curl's `-H` syntax).
pub fn parse_header(s: &str) -> Result<(String, String), String> {
    let (name, value) = s.split_once(':').ok_or_else(|| format!("header `{s}` must look like `Name: value`"))?;
    let name = name.trim();
    let valid_name = !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !valid_name {
        return Err(format!("`{name}` is not a valid header name"));
    }
    let value = value.trim();
    if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        return Err("header values must not contain line breaks".into());
    }
    Ok((name.to_string(), value.to_string()))
}

/// A SHA-256 digest given as 64 hex digits; returned lower-case.
pub fn parse_sha256(s: &str) -> Result<String, String> {
    let s = s.trim();
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err("expected 64 hexadecimal digits".into())
    }
}

/// Classic download-manager switches understood for compatibility.
const SWITCHES: &[&str] = &["/d", "/p", "/f", "/n", "/a", "/q", "/h", "/s"];

fn switch_of(arg: &OsString) -> Option<String> {
    let s = arg.to_str()?.to_ascii_lowercase();
    SWITCHES.contains(&s.as_str()).then_some(s)
}

/// Rewrite `spate-cli /d URL [/p dir] [/f name] [/n] [/a]` (also accepted
/// after `add`) into the equivalent `add` invocation. Returns the new argv
/// and warnings about switches that have no equivalent.
///
/// * `/d URL`  download URL      * `/p dir`  save folder   * `/f name` file name
/// * `/n`      silent (no dialog) * `/a`      add to queue without starting
/// * `/s`      bring Spate up (starting the queue is up to the app)
/// * `/q`, `/h` (exit / hang up when done) are accepted and ignored.
pub fn translate_switches(args: Vec<OsString>) -> (Vec<OsString>, Vec<String>) {
    let tail_start = match args.get(1) {
        Some(a) if switch_of(a).is_some() => 1,
        Some(a) if a == "add" && args[2..].iter().any(|a| switch_of(a).is_some()) => 2,
        _ => return (args, Vec::new()),
    };
    let mut out: Vec<OsString> = vec![args[0].clone(), "add".into()];
    let mut warnings = Vec::new();
    let mut saw_url = false;
    let mut start_queue = false;
    let mut it = args.into_iter().skip(tail_start);
    while let Some(arg) = it.next() {
        let Some(sw) = switch_of(&arg) else {
            out.push(arg);
            continue;
        };
        match sw.as_str() {
            "/d" | "/p" | "/f" => {
                let Some(value) = it.next() else {
                    warnings.push(format!("{sw} needs a value"));
                    continue;
                };
                match sw.as_str() {
                    "/d" => saw_url = true,
                    "/p" => out.push("--dir".into()),
                    _ => out.push("--name".into()),
                }
                out.push(value);
            }
            "/n" => out.push("--silent".into()),
            "/a" => out.push("--queue".into()),
            "/s" => start_queue = true,
            _ => warnings.push(format!("{sw} is accepted for compatibility and ignored")),
        }
    }
    if !saw_url && start_queue {
        return (vec![out[0].clone(), "show".into()], warnings);
    }
    if start_queue {
        warnings.push("/s is ignored when a URL is given".into());
    }
    (out, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("500K"), Ok(500 * 1024));
        assert_eq!(parse_size("2M"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_size("2m"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_size("1.5G"), Ok(1024 * 1024 * 1024 * 3 / 2));
        assert_eq!(parse_size("750KiB/s"), Ok(750 * 1024));
        assert_eq!(parse_size(" 1048576 "), Ok(1_048_576));
        assert_eq!(parse_size("10 MB"), Ok(10 << 20));
        assert_eq!(parse_size("0"), Ok(0));
        assert!(parse_size("").is_err());
        assert!(parse_size("M").is_err());
        assert!(parse_size("12X").is_err());
        assert!(parse_size("-5K").is_err());
        assert!(parse_size("99999999999999999999T").is_err());
    }

    #[test]
    fn headers() {
        assert_eq!(parse_header("Authorization: Bearer a:b"), Ok(("Authorization".into(), "Bearer a:b".into())));
        assert_eq!(parse_header("X-Empty:"), Ok(("X-Empty".into(), String::new())));
        assert!(parse_header("no colon").is_err());
        assert!(parse_header(": value").is_err());
        assert!(parse_header("Bad Name: v").is_err());
        assert!(parse_header("X: a\r\nInjected: 1").is_err());
    }

    #[test]
    fn sha256() {
        let h = "A".repeat(64);
        assert_eq!(parse_sha256(&h), Ok("a".repeat(64)));
        assert!(parse_sha256("abc").is_err());
        assert!(parse_sha256(&"g".repeat(64)).is_err());
    }

    fn argv(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn classic_switches() {
        let (a, w) = translate_switches(argv(&["cli", "/d", "http://x/y.zip", "/p", "C:\\dl", "/f", "y.zip", "/n", "/a"]));
        assert_eq!(a, argv(&["cli", "add", "http://x/y.zip", "--dir", "C:\\dl", "--name", "y.zip", "--silent", "--queue"]));
        assert!(w.is_empty());

        let (a, w) = translate_switches(argv(&["cli", "/N", "/D", "http://x/z", "/q", "/h"]));
        assert_eq!(a, argv(&["cli", "add", "--silent", "http://x/z"]));
        assert_eq!(w.len(), 2);

        let (a, _) = translate_switches(argv(&["cli", "add", "/d", "http://x/z", "--referer", "http://r/"]));
        assert_eq!(a, argv(&["cli", "add", "http://x/z", "--referer", "http://r/"]));

        let (a, _) = translate_switches(argv(&["cli", "/s"]));
        assert_eq!(a, argv(&["cli", "show"]));

        // Normal invocations are left alone.
        let normal = argv(&["cli", "get", "http://x/y", "-d", "/p"]);
        assert_eq!(translate_switches(normal.clone()).0, normal);
        let normal = argv(&["cli", "add", "http://x/y"]);
        assert_eq!(translate_switches(normal.clone()).0, normal);
    }
}
