//! File naming (ENGINE-SPEC §3) and categories (§13.1). Pure functions,
//! no I/O.

mod category;
mod disposition;
mod mime;
#[cfg(test)]
mod tests;

use std::collections::HashSet;

use reqwest::Url;

use crate::probe::ProbeInfo;

pub use category::{Category, ExtraExtensions, categorize, normalize_extension};

/// Longest final name in bytes: 255 minus the longest suffix the engine adds
/// (`.dhow.tmp` during sidecar compaction, §5.3). UTF-8 bytes never undercount
/// the UTF-16 units NTFS limits, so this holds everywhere.
pub const MAX_NAME_BYTES: usize = 255 - ".dhow.tmp".len();

/// Treated as one extension, so collisions become `x (2).tar.gz`.
const COMPOUND_EXTENSIONS: &[&str] = &[".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"];
const WINDOWS_FORBIDDEN: &[char] = &['\\', ':', '*', '?', '"', '<', '>', '|'];
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Naming rules of the volume the file lands on. The shell knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetFs {
    Posix,
    /// Also FAT/exFAT/SMB volumes on any OS.
    Windows,
}

impl TargetFs {
    pub fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }
}

/// The full §3 rule: [`base_name`] for the probe result, then
/// [`deduplicate`] against the names already in the target directory.
pub fn suggest_name<S: AsRef<str>>(
    info: &ProbeInfo,
    original_url: &Url,
    fs: TargetFs,
    existing: &[S],
) -> String {
    let base = base_name(
        info.content_disposition.as_deref(),
        info.content_type.as_deref(),
        &info.effective_url,
        original_url,
        fs,
    );
    deduplicate(&base, existing)
}

/// Picks, cleans and bounds a name without looking at the directory.
pub fn base_name(
    content_disposition: Option<&str>,
    content_type: Option<&str>,
    effective_url: &Url,
    original_url: &Url,
    fs: TargetFs,
) -> String {
    let (extended, plain) = content_disposition
        .map(disposition::filenames)
        .unwrap_or_default();
    let host = effective_url.host_str().or(original_url.host_str());
    let candidates = [
        extended.map(|n| strip_path(&n).to_owned()),
        plain.map(|n| strip_path(&n).to_owned()),
        last_segment(effective_url),
        last_segment(original_url),
        host.map(|h| format!("{h}-download")),
        Some("download".to_owned()),
    ];
    let type_ext = content_type.and_then(mime::extension);
    for raw in candidates.into_iter().flatten() {
        let mut name = clean(&raw, fs);
        if name.is_empty() {
            continue;
        }
        if let (Some(ext), "") = (type_ext, split(&name).1) {
            name = format!("{name}.{ext}");
        }
        return fit(&unreserve(name, fs), "");
    }
    unreachable!("\"download\" survives cleaning")
}

/// A user-supplied name made safe for `fs` (same rules as [`base_name`]),
/// or `None` if nothing usable is left.
pub fn sanitize(name: &str, fs: TargetFs) -> Option<String> {
    let cleaned = clean(name, fs);
    (!cleaned.is_empty()).then(|| fit(&unreserve(cleaned, fs), ""))
}

/// `name`, or `name (2)`, `name (3)`… before the extension: the first that
/// matches no existing name, `.part` or `.dhow` (case-insensitively, as on
/// macOS and Windows volumes).
pub fn deduplicate<S: AsRef<str>>(name: &str, existing: &[S]) -> String {
    let taken: HashSet<String> = existing.iter().map(|n| n.as_ref().to_lowercase()).collect();
    let free = |candidate: &str| {
        let c = candidate.to_lowercase();
        ["", ".part", ".dhow"]
            .iter()
            .all(|suffix| !taken.contains(&format!("{c}{suffix}")))
    };
    if free(name) {
        return name.to_owned();
    }
    (2u64..)
        .map(|n| fit(name, &format!(" ({n})")))
        .find(|c| free(c))
        .expect("finitely many names are taken")
}

/// `(stem, extension-with-dot)`. An extension is 1–10 ASCII alphanumerics
/// after the last dot of a non-empty stem, or one of the compound ones.
pub(crate) fn split(name: &str) -> (&str, &str) {
    let lower = name.to_ascii_lowercase();
    for ext in COMPOUND_EXTENSIONS {
        if lower.len() > ext.len() && lower.ends_with(ext) {
            return name.split_at(name.len() - ext.len());
        }
    }
    if let Some(i) = name.rfind('.').filter(|&i| i > 0) {
        let ext = &name[i + 1..];
        if (1..=10).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return name.split_at(i);
        }
    }
    (name, "")
}

/// Removes characters the target can't hold, surrounding whitespace and
/// leading dots (a download must not become a hidden file).
fn clean(raw: &str, fs: TargetFs) -> String {
    let kept: String = raw
        .chars()
        .filter(|&c| {
            !c.is_control()
                && c != '/'
                && !(fs == TargetFs::Windows && WINDOWS_FORBIDDEN.contains(&c))
        })
        .collect();
    let mut s = kept.trim().trim_start_matches('.').trim_start();
    if fs == TargetFs::Windows {
        s = s.trim_end_matches(['.', ' ']);
    }
    s.to_owned()
}

/// Windows device names are reserved with any extension: `con.txt` → `_con.txt`.
fn unreserve(name: String, fs: TargetFs) -> String {
    if fs != TargetFs::Windows {
        return name;
    }
    let device = name.split('.').next().unwrap_or_default().trim_end();
    if WINDOWS_RESERVED
        .iter()
        .any(|r| r.eq_ignore_ascii_case(device))
    {
        format!("_{name}")
    } else {
        name
    }
}

/// `stem + suffix + ext`, truncating the stem at a char boundary so the
/// result fits [`MAX_NAME_BYTES`].
fn fit(name: &str, suffix: &str) -> String {
    let (stem, ext) = split(name);
    let budget = MAX_NAME_BYTES.saturating_sub(ext.len() + suffix.len());
    let mut stem = stem;
    if stem.len() > budget {
        let mut end = budget;
        while !stem.is_char_boundary(end) {
            end -= 1;
        }
        stem = stem[..end].trim_end_matches(['.', ' ']);
    }
    format!("{stem}{suffix}{ext}")
}

/// Content-Disposition values may carry paths, either separator.
fn strip_path(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

fn last_segment(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    Some(decode_text(percent_decode_lenient(segment)))
}

/// UTF-8 if valid, else ISO-8859-1 (what old servers mean by raw bytes).
pub(crate) fn decode_text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|e| e.into_bytes().into_iter().map(char::from).collect())
}

/// Strict: any malformed escape fails (RFC 8187).
fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = (bytes.next()? as char).to_digit(16)?;
            let lo = (bytes.next()? as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
        } else {
            out.push(b);
        }
    }
    Some(out)
}

/// Lenient: a `%` not followed by two hex digits is literal (URL paths).
fn percent_decode_lenient(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |j: usize| b.get(j).and_then(|&c| (c as char).to_digit(16));
        match (b[i], hex(i + 1), hex(i + 2)) {
            (b'%', Some(hi), Some(lo)) => {
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}
