//! Grouping keys (ENGINE-SPEC §13.2). Pure.

use super::split;

/// Shorter keys (normalized stem + extension) never auto-group.
const MIN_KEY_CHARS: usize = 4;
/// Words a trailing sequence number may follow (`Part 2`, `ep03`).
const SEQUENCE_WORDS: &[&str] = &[
    "part", "pt", "cd", "disc", "disk", "vol", "episode", "ep", "e",
];

/// `(key, display name)` for a file name, or `None` if it never
/// auto-groups. The key is the normalized stem, lowercased, plus the
/// lowercased extension; the display name is the stem in its own casing.
pub fn group_key(name: &str) -> Option<(String, String)> {
    let (stem, ext) = split(name);
    // `x.7z.001`: a numeric extension is the sequence, not the type. A
    // collision mark goes before it (`x.7z (2).001`).
    if ext.len() > 1 && ext[1..].bytes().all(|b| b.is_ascii_digit()) {
        let unmarked = strip_bracketed(stem.trim_end());
        return finish(&format!("{unmarked}{ext}"), "");
    }
    finish(stem, ext)
}

fn finish(stem: &str, ext: &str) -> Option<(String, String)> {
    let display = normalize(stem);
    if display.is_empty() {
        return None;
    }
    let key = format!("{}{}", display.to_lowercase(), ext.to_ascii_lowercase());
    (key.chars().count() >= MIN_KEY_CHARS).then_some((key, display))
}

/// Separators collapsed to one space, then a trailing collision mark
/// (`(2)`, `[02]`, up to 3 digits), `of N` and sequence token stripped, in that order.
fn normalize(stem: &str) -> String {
    let mut s = String::with_capacity(stem.len());
    for c in stem.chars() {
        if matches!(c, '.' | '_' | '-') || c.is_whitespace() {
            if !s.ends_with(' ') {
                s.push(' ');
            }
        } else {
            s.push(c);
        }
    }
    let s = strip_bracketed(s.trim());
    let s = strip_of(s);
    strip_sequence(s).trim_end().to_owned()
}

/// `(rest, how many trailing ASCII digits)`.
fn trailing_digits(s: &str) -> (&str, usize) {
    let rest = s.trim_end_matches(|c: char| c.is_ascii_digit());
    (rest, s.len() - rest.len())
}

/// `s` without a trailing `word` (ASCII case-insensitive) that starts a
/// word.
fn strip_word<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let at = s.len().checked_sub(word.len())?;
    let tail = s.get(at..)?;
    let rest = &s[..at];
    (tail.eq_ignore_ascii_case(word) && (rest.is_empty() || rest.ends_with(' '))).then_some(rest)
}

fn strip_bracketed(s: &str) -> &str {
    for (open, close) in [('(', ')'), ('[', ']')] {
        if let Some(inner) = s.strip_suffix(close) {
            let (rest, n) = trailing_digits(inner);
            // Not 4 digits: `Film (1999)` is a year.
            if let (true, Some(rest)) = ((1..=3).contains(&n), rest.strip_suffix(open)) {
                return rest.trim_end();
            }
        }
    }
    s
}

fn strip_of(s: &str) -> &str {
    let (rest, n) = trailing_digits(s);
    match strip_word(rest.trim_end(), "of") {
        Some(rest) if n > 0 => rest.trim_end(),
        _ => s,
    }
}

/// 1–4 trailing digits, with the word before them if it is a
/// [`SEQUENCE_WORDS`] one or `s<n>e` (`s01e02`).
fn strip_sequence(s: &str) -> &str {
    let (rest, n) = trailing_digits(s);
    if !(1..=4).contains(&n) {
        return s;
    }
    let word = rest.trim_end();
    if let Some(r) = SEQUENCE_WORDS.iter().find_map(|w| strip_word(word, w)) {
        return r;
    }
    if let Some(season) = word.strip_suffix(['e', 'E']) {
        let (before, n) = trailing_digits(season);
        if let (true, Some(r)) = (n > 0, strip_word(before, "s")) {
            return r;
        }
    }
    rest
}
