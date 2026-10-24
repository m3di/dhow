//! Categories (ENGINE-SPEC §13.1): the final name's extension first,
//! `Content-Type` second. The built-in table is `categories.toml`; the
//! user's `extra_extensions` win over it.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use super::mime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Video,
    Audio,
    Image,
    Document,
    Archive,
    Software,
    Code,
    Data,
    Other,
}

impl Category {
    pub const ALL: [Self; 9] = [
        Self::Video,
        Self::Audio,
        Self::Image,
        Self::Document,
        Self::Archive,
        Self::Software,
        Self::Code,
        Self::Data,
        Self::Other,
    ];

    /// Also the `downloads.category` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Image => "image",
            Self::Document => "document",
            Self::Archive => "archive",
            Self::Software => "software",
            Self::Code => "code",
            Self::Data => "data",
            Self::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|c| c.as_str().eq_ignore_ascii_case(s))
    }
}

/// User additions: extensions (normalized, see [`normalize_extension`]) per
/// category.
pub type ExtraExtensions = BTreeMap<Category, Vec<String>>;

static BUILT_IN: LazyLock<HashMap<String, Category>> = LazyLock::new(|| {
    let table: BTreeMap<Category, Vec<String>> =
        toml::from_str(include_str!("categories.toml")).expect("categories.toml parses");
    table
        .into_iter()
        .flat_map(|(c, exts)| exts.into_iter().map(move |e| (e, c)))
        .collect()
});

/// The category of a file called `name` served as `content_type`.
///
/// Extensions match case-insensitively, longest first, so a user's
/// `tar.gz` beats `gz`; at each length the user's extras beat the table.
/// Without a known extension, the `Content-Type`'s usual extension decides,
/// then its top-level type (`video/*`, `audio/*`, `image/*`).
pub fn categorize(name: &str, content_type: Option<&str>, extra: &ExtraExtensions) -> Category {
    if let Some(c) = categorize_name(name, extra) {
        return c;
    }
    let Some(ct) = content_type else {
        return Category::Other;
    };
    if let Some(c) = mime::extension(ct).and_then(|ext| lookup(ext, extra)) {
        return c;
    }
    let essence = ct.trim_start().to_ascii_lowercase();
    match essence.split('/').next() {
        Some("video") => Category::Video,
        Some("audio") => Category::Audio,
        Some("image") => Category::Image,
        _ => Category::Other,
    }
}

/// The category `name`'s extension gives, if it is a known one.
pub fn categorize_name(name: &str, extra: &ExtraExtensions) -> Option<Category> {
    let lower = name.to_lowercase();
    lower
        .match_indices('.')
        .filter(|&(i, _)| i > 0)
        .map(|(i, _)| &lower[i + 1..])
        .filter(|ext| !ext.is_empty())
        .find_map(|ext| lookup(ext, extra))
}

fn lookup(ext: &str, extra: &ExtraExtensions) -> Option<Category> {
    extra
        .iter()
        .find(|(_, exts)| exts.iter().any(|e| e == ext))
        .map(|(c, _)| *c)
        .or_else(|| BUILT_IN.get(ext).copied())
}

/// `".MKV"` → `"mkv"`; `None` if nothing usable is left. Inner dots are
/// kept for compound extensions (`tar.gz`).
pub fn normalize_extension(ext: &str) -> Option<String> {
    let e = ext.trim().trim_start_matches('.').to_lowercase();
    let ok = !e.is_empty()
        && !e.ends_with('.')
        && !e.contains("..")
        && e.chars().all(|c| c.is_alphanumeric() || "._-+".contains(c));
    ok.then_some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(name: &str) -> Category {
        categorize(name, None, &ExtraExtensions::new())
    }

    #[test]
    fn built_in_table_is_normalized_and_unambiguous() {
        let table: BTreeMap<Category, Vec<String>> =
            toml::from_str(include_str!("categories.toml")).unwrap();
        assert!(!table.contains_key(&Category::Other));
        let mut seen = HashMap::new();
        for (c, exts) in &table {
            for e in exts {
                assert_eq!(normalize_extension(e).as_deref(), Some(e.as_str()));
                assert_eq!(seen.insert(e.clone(), *c), None, "{e} is in two categories");
            }
        }
    }

    #[test]
    fn extension_decides() {
        assert_eq!(cat("Movie.mkv"), Category::Video);
        assert_eq!(cat("song.MP3"), Category::Audio);
        assert_eq!(cat("photo.JpEg"), Category::Image);
        assert_eq!(cat("report.pdf"), Category::Document);
        assert_eq!(cat("setup.exe"), Category::Software);
        assert_eq!(cat("main.rs"), Category::Code);
        assert_eq!(cat("rows.csv"), Category::Data);
        assert_eq!(cat("src.tar.gz"), Category::Archive);
        assert_eq!(cat("Movie.Part1.rar"), Category::Archive);
        assert_eq!(cat("report (2).pdf"), Category::Document);
        assert_eq!(cat("thing.unknownext"), Category::Other);
        assert_eq!(cat("README"), Category::Other);
        // A leading dot is not an extension.
        assert_eq!(cat(".mp4"), Category::Other);
    }

    #[test]
    fn content_type_is_the_fallback() {
        let none = ExtraExtensions::new();
        let ct = |name, ct| categorize(name, Some(ct), &none);
        assert_eq!(ct("clip", "video/mp4"), Category::Video);
        assert_eq!(
            ct("clip", "Application/PDF; charset=binary"),
            Category::Document
        );
        assert_eq!(ct("clip", "audio/x-something-new"), Category::Audio);
        assert_eq!(ct("clip", "image/x-new"), Category::Image);
        assert_eq!(ct("clip", "application/octet-stream"), Category::Other);
        // The extension wins over the type.
        assert_eq!(ct("clip.zip", "video/mp4"), Category::Archive);
        // An unknown extension falls through to the type.
        assert_eq!(ct("clip.xyz123", "video/mp4"), Category::Video);
    }

    #[test]
    fn extras_extend_and_override() {
        let mut extra = ExtraExtensions::new();
        extra.insert(Category::Video, vec!["xyz".into()]);
        extra.insert(Category::Data, vec!["mp4".into()]);
        extra.insert(Category::Software, vec!["tar.gz".into()]);
        let c = |name, ct| categorize(name, ct, &extra);
        assert_eq!(c("a.XYZ", None), Category::Video);
        assert_eq!(c("a.mp4", None), Category::Data);
        assert_eq!(c("a.tar.gz", None), Category::Software);
        assert_eq!(c("a.gz", None), Category::Archive);
        // Extras also apply to the type's extension.
        assert_eq!(c("a", Some("video/mp4")), Category::Data);
    }

    #[test]
    fn normalizes_extensions() {
        assert_eq!(normalize_extension(" .MKV ").as_deref(), Some("mkv"));
        assert_eq!(normalize_extension("tar.gz").as_deref(), Some("tar.gz"));
        assert_eq!(normalize_extension("."), None);
        assert_eq!(normalize_extension(""), None);
        assert_eq!(normalize_extension("a/b"), None);
        assert_eq!(normalize_extension("a b"), None);
        assert_eq!(normalize_extension("gz."), None);
    }

    #[test]
    fn names_round_trip() {
        for c in Category::ALL {
            assert_eq!(Category::parse(c.as_str()), Some(c));
        }
        assert_eq!(Category::parse("Video"), Some(Category::Video));
        assert_eq!(Category::parse("films"), None);
    }
}
