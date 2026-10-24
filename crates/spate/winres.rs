// Build-script helper shared by `spate` and `spate-cli` (both `include!` it):
// renders the app icon into a multi-size .ico and embeds it, with version
// information, as Windows resources so Explorer shows the icon and the file's
// Properties → Details page.

include!("src/icon_art.rs");

const ICON_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

/// Embed the icon and a VERSIONINFO block into this package's binaries.
/// Does nothing when the target is not Windows.
fn embed_windows_resources(description: &str, original_filename: &str) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set for build scripts"));
    let ico = out_dir.join("spate.ico");
    std::fs::write(&ico, ico_file(&ICON_SIZES)).expect("write spate.ico");

    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let version = env("CARGO_PKG_VERSION");
    let numeric = format!("{},{},{},0", env("CARGO_PKG_VERSION_MAJOR"), env("CARGO_PKG_VERSION_MINOR"), env("CARGO_PKG_VERSION_PATCH"));
    let ico_path = ico.display().to_string().replace('\\', "\\\\");
    // Numeric constants instead of <winver.h> so neither windres nor rc.exe
    // needs SDK headers: VOS_NT_WINDOWS32 = 0x40004, VFT_APP = 0x1.
    let rc = format!(
        r#"1 ICON "{ico_path}"

1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3f
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "Spate contributors"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{original_filename}"
      VALUE "LegalCopyright", "MIT License"
      VALUE "OriginalFilename", "{original_filename}"
      VALUE "ProductName", "Spate"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let rc_path = out_dir.join("spate.rc");
    std::fs::write(&rc_path, rc).expect("write spate.rc");
    embed_resource::compile(&rc_path, embed_resource::NONE).manifest_optional().expect("compile Windows resources");
}

/// An .ico with one uncompressed 32-bit image per size (alpha carries the
/// transparency; the AND masks are left empty).
fn ico_file(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| ico_image(s)).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]); // reserved, type 1 = icon
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&size, image) in sizes.iter().zip(&images) {
        let dim = if size >= 256 { 0 } else { size as u8 }; // 0 means 256
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(image.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += image.len() as u32;
    }
    for image in images {
        out.extend(image);
    }
    out
}

/// BITMAPINFOHEADER + bottom-up BGRA rows + AND mask, as stored inside an .ico.
fn ico_image(size: u32) -> Vec<u8> {
    let n = size as usize;
    let rgba = app_icon_rgba(size);
    let mask_stride = n.div_ceil(32) * 4;
    let mut out = Vec::with_capacity(40 + n * n * 4 + mask_stride * n);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&(2 * size as i32).to_le_bytes()); // colour image + mask
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]); // BI_RGB; size, resolution and palette unused
    for row in rgba.chunks_exact(n * 4).rev() {
        for px in row.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    out.resize(out.len() + mask_stride * n, 0);
    out
}
