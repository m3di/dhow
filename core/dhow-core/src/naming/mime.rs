//! `Content-Type` → extension. The curated table wins; `mime_guess` is used
//! only when it knows exactly one extension, since its first-of-many pick is
//! alphabetical (`image/jpeg` → `jfif`, `text/plain` → `asm`).

/// Types that say nothing about the content.
const OPAQUE: &[&str] = &[
    "application/octet-stream",
    "binary/octet-stream",
    "application/binary",
    "application/download",
    "application/force-download",
    "application/x-download",
    "application/unknown",
];

const TABLE: &[(&str, &str)] = &[
    ("application/epub+zip", "epub"),
    ("application/gzip", "gz"),
    ("application/javascript", "js"),
    ("application/json", "json"),
    ("application/msword", "doc"),
    ("application/pdf", "pdf"),
    ("application/vnd.android.package-archive", "apk"),
    ("application/vnd.ms-excel", "xls"),
    ("application/vnd.ms-powerpoint", "ppt"),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pptx",
    ),
    (
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsx",
    ),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
    ),
    ("application/vnd.rar", "rar"),
    ("application/wasm", "wasm"),
    ("application/x-7z-compressed", "7z"),
    ("application/x-apple-diskimage", "dmg"),
    ("application/x-bittorrent", "torrent"),
    ("application/x-bzip2", "bz2"),
    ("application/x-gzip", "gz"),
    ("application/x-iso9660-image", "iso"),
    ("application/x-msdownload", "exe"),
    ("application/x-msi", "msi"),
    ("application/x-rar-compressed", "rar"),
    ("application/x-tar", "tar"),
    ("application/x-xz", "xz"),
    ("application/x-zip-compressed", "zip"),
    ("application/xml", "xml"),
    ("application/zip", "zip"),
    ("application/zstd", "zst"),
    ("audio/aac", "aac"),
    ("audio/flac", "flac"),
    ("audio/mp4", "m4a"),
    ("audio/mpeg", "mp3"),
    ("audio/ogg", "ogg"),
    ("audio/opus", "opus"),
    ("audio/wav", "wav"),
    ("audio/x-flac", "flac"),
    ("audio/x-wav", "wav"),
    ("image/avif", "avif"),
    ("image/bmp", "bmp"),
    ("image/gif", "gif"),
    ("image/heic", "heic"),
    ("image/jpeg", "jpg"),
    ("image/png", "png"),
    ("image/svg+xml", "svg"),
    ("image/tiff", "tiff"),
    ("image/webp", "webp"),
    ("image/x-icon", "ico"),
    ("text/css", "css"),
    ("text/csv", "csv"),
    ("text/html", "html"),
    ("text/javascript", "js"),
    ("text/markdown", "md"),
    ("text/plain", "txt"),
    ("text/xml", "xml"),
    ("video/mp4", "mp4"),
    ("video/mpeg", "mpg"),
    ("video/quicktime", "mov"),
    ("video/webm", "webm"),
    ("video/x-matroska", "mkv"),
    ("video/x-msvideo", "avi"),
];

/// Extension (without dot) for a `Content-Type` header value, if known.
pub(super) fn extension(content_type: &str) -> Option<&'static str> {
    let essence = content_type.split(';').next()?.trim().to_ascii_lowercase();
    if essence.is_empty() || OPAQUE.contains(&essence.as_str()) {
        return None;
    }
    if let Ok(i) = TABLE.binary_search_by(|(t, _)| t.cmp(&essence.as_str())) {
        return Some(TABLE[i].1);
    }
    match mime_guess::get_mime_extensions_str(&essence) {
        Some([only]) => Some(only),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_for_binary_search() {
        assert!(TABLE.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn lookups() {
        assert_eq!(extension("image/jpeg"), Some("jpg"));
        assert_eq!(extension("Text/Plain; charset=utf-8"), Some("txt"));
        assert_eq!(extension("application/octet-stream"), None);
        assert_eq!(extension("application/x-msdownload"), Some("exe"));
        // mime_guess with a single extension.
        assert_eq!(
            extension("application/vnd.oasis.opendocument.text"),
            Some("odt")
        );
        // mime_guess with many: no guess.
        assert_eq!(extension("application/x-msdos-program"), None);
        assert_eq!(extension("nonsense/type"), None);
        assert_eq!(extension(""), None);
    }
}
