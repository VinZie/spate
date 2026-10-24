//! File categories, used for default save folders and the sidebar filter.

use serde::{Deserialize, Serialize};

use crate::naming::split_ext;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Category {
    Compressed,
    Documents,
    Music,
    Programs,
    Video,
    Other,
}

impl Category {
    pub const ALL: [Category; 6] =
        [Self::Compressed, Self::Documents, Self::Music, Self::Programs, Self::Video, Self::Other];

    pub fn label(self) -> &'static str {
        match self {
            Self::Compressed => "Compressed",
            Self::Documents => "Documents",
            Self::Music => "Music",
            Self::Programs => "Programs",
            Self::Video => "Video",
            Self::Other => "Other",
        }
    }

    /// Sub-folder of the download directory used for this category.
    pub fn folder(self) -> &'static str {
        match self {
            Self::Other => "General",
            other => other.label(),
        }
    }

    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Compressed => &[
                "zip", "rar", "7z", "gz", "tgz", "bz2", "xz", "zst", "tar", "arj", "lzh", "ace", "cab", "sit",
                "sitx", "sea", "r00", "r01", "001", "iso", "img", "dmg",
            ],
            Self::Documents => &[
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "pps", "odt", "ods", "odp", "rtf", "txt",
                "epub", "mobi", "csv",
            ],
            Self::Music => &["mp3", "wav", "wma", "aac", "m4a", "flac", "ogg", "opus", "aif", "aiff", "mpa", "ra"],
            Self::Programs => &["exe", "msi", "msix", "appx", "apk", "deb", "rpm", "appimage", "pkg", "bat", "jar"],
            Self::Video => &[
                "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "mpe", "3gp", "ts", "m2ts",
                "ogv", "asf", "rm", "rmvb", "qt",
            ],
            Self::Other => &[],
        }
    }

    pub fn for_file_name(name: &str) -> Self {
        let ext = split_ext(name).1.to_ascii_lowercase();
        if ext.is_empty() {
            return Self::Other;
        }
        Self::ALL.into_iter().find(|c| c.extensions().contains(&ext.as_str())).unwrap_or(Self::Other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_extension() {
        assert_eq!(Category::for_file_name("setup.EXE"), Category::Programs);
        assert_eq!(Category::for_file_name("movie.final.mkv"), Category::Video);
        assert_eq!(Category::for_file_name("archive.tar.gz"), Category::Compressed);
        assert_eq!(Category::for_file_name("README"), Category::Other);
    }
}
