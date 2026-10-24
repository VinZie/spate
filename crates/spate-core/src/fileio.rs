//! Output-file helpers: sparse preallocation and positional writes.
//!
//! All connections write straight into the final file at their own offsets,
//! so there is no "assembling parts" step when a download finishes. On NTFS,
//! writing far beyond the current valid-data length makes the OS zero-fill
//! the gap first, which would stall every segment that starts deep inside a
//! large file; marking the file sparse avoids that.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Create (or truncate) `path` and size it to `len` bytes without writing them.
pub fn create_preallocated(path: &Path, len: Option<u64>) -> io::Result<File> {
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
    if let Some(len) = len {
        set_sparse(&file, true);
        file.set_len(len)?;
    }
    Ok(file)
}

/// Open an existing partial download for resuming.
pub fn open_partial(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

/// Write all of `buf` at `offset` without touching any shared cursor.
pub fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = write_at(file, buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write to file"));
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

/// Fill `buf` from `offset` without touching any shared cursor.
pub fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = read_at(file, buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past the end of the file"));
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

/// Toggle the NTFS sparse attribute. Failure is harmless (e.g. FAT32 or a
/// network share), it only costs the zero-fill optimisation.
#[cfg(windows)]
pub fn set_sparse(file: &File, sparse: bool) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{FILE_SET_SPARSE_BUFFER, FSCTL_SET_SPARSE};

    let input = FILE_SET_SPARSE_BUFFER { SetSparse: sparse };
    let mut returned = 0u32;
    // SAFETY: the handle is valid for the lifetime of `file`; the input buffer
    // is a properly sized FILE_SET_SPARSE_BUFFER and no output is requested.
    unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            &input as *const _ as *const _,
            size_of::<FILE_SET_SPARSE_BUFFER>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        );
    }
}

/// Unix filesystems create holes automatically when a file is extended.
#[cfg(unix)]
pub fn set_sparse(_file: &File, _sparse: bool) {}

/// Mark a downloaded file as coming from the internet by writing the
/// `Zone.Identifier` stream browsers write, so SmartScreen, Office Protected
/// View and the macro block treat it like any other download. Write it on the
/// `.part` file before the final rename; the stream moves with the file.
///
/// `user:password@` and `#fragment` are stripped from both URLs, and a URL
/// that isn't http(s) is recorded as `about:internet`, as browsers do for
/// `blob:` and `data:` downloads. Volumes without alternate data streams
/// (FAT32, exFAT, some network shares) return an error the caller can ignore.
#[cfg(windows)]
pub fn mark_from_internet(path: &Path, host_url: &str, referrer: Option<&str>) -> io::Result<()> {
    let mut text = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if let Some(r) = referrer.and_then(zone_url) {
        text.push_str(&format!("ReferrerUrl={r}\r\n"));
    }
    text.push_str(&format!("HostUrl={}\r\n", zone_url(host_url).unwrap_or_else(|| "about:internet".into())));
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    std::fs::write(Path::new(&stream), text)
}

/// Other platforms have no equivalent the OS acts on.
#[cfg(not(windows))]
pub fn mark_from_internet(_path: &Path, _host_url: &str, _referrer: Option<&str>) -> io::Result<()> {
    Ok(())
}

/// An http(s) URL without credentials or fragment, as recorded in the zone
/// stream. The URL parser has already dropped CR, LF and tabs.
#[cfg(windows)]
fn zone_url(url: &str) -> Option<String> {
    let mut u = reqwest::Url::parse(url.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.set_fragment(None);
    Some(u.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[cfg(windows)]
    #[test]
    fn zone_identifier_marks_the_file_without_credentials() {
        let dir = std::env::temp_dir().join(format!("spate-motw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("setup.exe.part");
        std::fs::write(&path, b"MZ").unwrap();
        mark_from_internet(&path, "https://user:secret@dl.example.com/setup.exe?x=1#frag", Some("https://example.com/page#top"))
            .unwrap();
        // The stream survives the rename that completes a download.
        let done = dir.join("setup.exe");
        std::fs::rename(&path, &done).unwrap();
        let zone = std::fs::read_to_string(format!("{}:Zone.Identifier", done.display())).unwrap();
        assert_eq!(
            zone,
            "[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://example.com/page\r\nHostUrl=https://dl.example.com/setup.exe?x=1\r\n"
        );
        assert!(!zone.contains("secret"));

        mark_from_internet(&done, "blob:https://example.com/uuid", None).unwrap();
        let zone = std::fs::read_to_string(format!("{}:Zone.Identifier", done.display())).unwrap();
        assert!(zone.ends_with("HostUrl=about:internet\r\n"), "{zone}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn positional_writes_land_at_offsets() {
        let dir = std::env::temp_dir().join(format!("spate-fileio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.bin");
        let f = create_preallocated(&path, Some(10)).unwrap();
        write_all_at(&f, b"world", 5).unwrap();
        write_all_at(&f, b"hello", 0).unwrap();
        let mut mid = [0u8; 4];
        read_exact_at(&f, &mut mid, 3).unwrap();
        assert_eq!(&mid, b"lowo");
        assert!(read_exact_at(&f, &mut mid, 8).is_err(), "reading past the end must fail");
        set_sparse(&f, false);
        drop(f);
        let mut s = String::new();
        File::open(&path).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "helloworld");
        std::fs::remove_dir_all(&dir).ok();
    }
}
