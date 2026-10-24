//! `info`: probe a URL with a one-byte range request.

use clap::Args;
use spate_core::http::{self, header_str};
use spate_core::{Settings, naming, paths};
use reqwest::header;
use serde::Serialize;

use crate::{EXIT_ERROR, EXIT_OK, NetArgs, fmt};

#[derive(Args, Debug)]
pub struct InfoArgs {
    #[arg(value_name = "URL")]
    pub url: String,
    /// Print JSON instead of text
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub net: NetArgs,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ProbeInfo {
    pub url: String,
    pub final_url: String,
    pub status: u16,
    pub file_name: String,
    pub size: Option<u64>,
    pub resumable: bool,
    pub content_type: Option<String>,
    pub last_modified: Option<String>,
    pub etag: Option<String>,
}

/// Interpret the response to `Range: bytes=0-0` the way the engine would.
pub fn interpret(
    url: &url::Url,
    final_url: &url::Url,
    status: u16,
    headers: &header::HeaderMap,
) -> Result<ProbeInfo, String> {
    let (size, resumable) = match status {
        206 => {
            let (first, _, total) = header_str(headers, header::CONTENT_RANGE)
                .and_then(http::parse_content_range)
                .ok_or("server sent a partial response without a valid Content-Range")?;
            if first != 0 {
                return Err("server ignored the requested range".into());
            }
            (total, total.is_some())
        }
        200 => {
            let total = header_str(headers, header::CONTENT_LENGTH).and_then(|v| v.parse().ok());
            let ranges = header_str(headers, header::ACCEPT_RANGES).is_some_and(|v| v.eq_ignore_ascii_case("bytes"));
            (total, ranges && total.is_some())
        }
        // `bytes */0`: an empty file cannot satisfy any range.
        416 => {
            let total = header_str(headers, header::CONTENT_RANGE)
                .and_then(|v| v.trim().strip_prefix("bytes */"))
                .and_then(|t| t.trim().parse().ok());
            (total, false)
        }
        s => return Err(format!("server responded with HTTP {s}")),
    };
    let content_type = header_str(headers, header::CONTENT_TYPE).map(str::to_string);
    let mut name = header_str(headers, header::CONTENT_DISPOSITION)
        .and_then(naming::from_content_disposition)
        .or_else(|| naming::from_url(final_url))
        .or_else(|| naming::from_url(url))
        .unwrap_or_else(|| "download".to_string());
    if naming::split_ext(&name).1.is_empty()
        && let Some(ext) = content_type.as_deref().and_then(naming::extension_for_mime)
    {
        name = format!("{name}.{ext}");
    }
    Ok(ProbeInfo {
        url: url.to_string(),
        final_url: final_url.to_string(),
        status,
        file_name: name,
        size,
        resumable,
        content_type,
        last_modified: header_str(headers, header::LAST_MODIFIED).map(str::to_string),
        etag: header_str(headers, header::ETAG).map(str::to_string),
    })
}

pub async fn probe(url: &str, net: &NetArgs) -> Result<ProbeInfo, String> {
    let parsed = url::Url::parse(url.trim()).map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("unsupported scheme `{}`", parsed.scheme()));
    }
    let settings = Settings::load(&paths::data_dir());
    let client = http::build_client(&net.net_config(settings.net)).map_err(|e| e.to_string())?;
    let resp = client
        .get(parsed.clone())
        .headers(http::header_map(&net.headers()))
        .header(header::RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|e| format!("request failed: {}", fmt::error_chain(&e)))?;
    // Only the headers matter; dropping the response closes the connection
    // even if the server ignored the range and started a full body.
    interpret(&parsed, resp.url(), resp.status().as_u16(), resp.headers())
}

pub async fn run(args: InfoArgs) -> u8 {
    let info = match probe(&args.url, &args.net).await {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_ERROR;
        }
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&info).unwrap_or_default());
        return EXIT_OK;
    }
    let size = match info.size {
        Some(n) => format!("{n} bytes ({})", fmt::bytes(n)),
        None => "unknown".into(),
    };
    println!("File name:     {}", info.file_name);
    println!("Size:          {size}");
    println!("Resumable:     {}", if info.resumable { "yes (multi-connection capable)" } else { "no (single connection)" });
    println!("Content type:  {}", info.content_type.as_deref().unwrap_or("unknown"));
    println!("Final URL:     {}", info.final_url);
    println!("HTTP status:   {}", info.status);
    if let Some(m) = &info.last_modified {
        println!("Last modified: {m}");
    }
    if let Some(e) = &info.etag {
        println!("ETag:          {e}");
    }
    EXIT_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn interprets_probe_responses() {
        let u = url::Url::parse("https://example.com/dl?id=3").unwrap();
        let f = url::Url::parse("https://cdn.example.com/files/setup.exe").unwrap();

        let i = interpret(&u, &f, 206, &headers(&[("content-range", "bytes 0-0/12345")])).unwrap();
        assert_eq!((i.size, i.resumable, i.file_name.as_str()), (Some(12345), true, "setup.exe"));

        let h = headers(&[("content-length", "99"), ("content-disposition", "attachment; filename=\"a b.zip\"")]);
        let i = interpret(&u, &f, 200, &h).unwrap();
        assert_eq!((i.size, i.resumable, i.file_name.as_str()), (Some(99), false, "a b.zip"));

        let h = headers(&[("content-length", "99"), ("accept-ranges", "bytes")]);
        assert!(interpret(&u, &f, 200, &h).unwrap().resumable);

        let root = url::Url::parse("https://example.com/").unwrap();
        let i = interpret(&root, &root, 200, &headers(&[("content-type", "application/pdf")])).unwrap();
        assert_eq!((i.size, i.file_name.as_str()), (None, "download.pdf"));

        assert_eq!(interpret(&u, &f, 416, &headers(&[("content-range", "bytes */0")])).unwrap().size, Some(0));
        assert!(interpret(&u, &f, 404, &HeaderMap::new()).is_err());
        assert!(interpret(&u, &f, 206, &headers(&[("content-range", "bytes 5-9/10")])).is_err());
    }
}
