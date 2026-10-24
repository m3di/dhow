//! `Content-Disposition` filename extraction (RFC 6266, RFC 8187).

/// `filename*` if it decodes (UTF-8 or ISO-8859-1 only), else `filename`.
/// Returned as sent, paths not yet stripped.
pub(super) fn filenames(value: &str) -> (Option<String>, Option<String>) {
    let mut extended = None;
    let mut plain = None;
    for (name, v) in params(value) {
        match name.as_str() {
            "filename*" if extended.is_none() => extended = ext_value(&v),
            "filename" if plain.is_none() => plain = Some(v),
            _ => {}
        }
    }
    (extended, plain)
}

/// Parameters after the disposition type, names lowercased, quoted strings
/// unescaped. Lenient: unquoted values may contain spaces, an unterminated
/// quote runs to the end.
fn params(value: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = match value.split_once(';') {
        Some((_, rest)) => rest,
        None => return out,
    };
    loop {
        rest = rest.trim_start_matches([' ', '\t', ';']);
        if rest.is_empty() {
            return out;
        }
        let end = rest.find(['=', ';']).unwrap_or(rest.len());
        let name = rest[..end].trim().to_ascii_lowercase();
        rest = &rest[end..];
        let Some(after_eq) = rest.strip_prefix('=') else {
            continue;
        };
        let after_eq = after_eq.trim_start();
        let (v, remaining) = match after_eq.strip_prefix('"') {
            Some(quoted) => unquote(quoted),
            None => {
                let end = after_eq.find(';').unwrap_or(after_eq.len());
                (after_eq[..end].trim().to_owned(), &after_eq[end..])
            }
        };
        out.push((name, v));
        rest = remaining;
    }
}

/// Reads a quoted-string body (opening quote already consumed). A backslash
/// escapes only `"` and `\`; elsewhere it is kept, because servers send
/// unescaped Windows paths (`"C:\dir\f.exe"`) that path stripping must see.
fn unquote(s: &str) -> (String, &str) {
    let mut out = String::new();
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some((_, '"' | '\\'))) => {
                out.push(chars.next().unwrap().1);
            }
            '"' => return (out, &s[i + 1..]),
            c => out.push(c),
        }
    }
    (out, "")
}

/// RFC 8187 `charset'language'pct-encoded`.
fn ext_value(v: &str) -> Option<String> {
    let mut parts = v.splitn(3, '\'');
    let (charset, _lang, encoded) = (parts.next()?, parts.next()?, parts.next()?);
    let bytes = super::percent_decode(encoded)?;
    match charset.to_ascii_lowercase().as_str() {
        "utf-8" => String::from_utf8(bytes).ok(),
        "iso-8859-1" => Some(bytes.into_iter().map(char::from).collect()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(v: &str) -> (Option<String>, Option<String>) {
        filenames(v)
    }

    fn s(v: &str) -> Option<String> {
        Some(v.to_owned())
    }

    #[test]
    fn plain_and_extended() {
        assert_eq!(f("attachment; filename=\"a.zip\""), (None, s("a.zip")));
        assert_eq!(f("attachment; filename=a.zip"), (None, s("a.zip")));
        assert_eq!(
            f("attachment; filename*=UTF-8''%E2%82%AC%20rates.txt; filename=\"EUR rates.txt\""),
            (s("€ rates.txt"), s("EUR rates.txt"))
        );
        assert_eq!(
            f("inline; FILENAME*=iso-8859-1'en'%A3%20rates"),
            (s("£ rates"), None)
        );
    }

    #[test]
    fn quoting() {
        assert_eq!(
            f(r#"attachment; filename="a \"q\" ; b.txt""#).1,
            s("a \"q\" ; b.txt")
        );
        assert_eq!(f("attachment; filename=\"open.txt").1, s("open.txt"));
        assert_eq!(
            f(r#"attachment; filename="C:\dir\f.exe""#).1,
            s(r"C:\dir\f.exe")
        );
        assert_eq!(f(r#"attachment; filename="a\\b""#).1, s(r"a\b"));
        assert_eq!(
            f("attachment; filename = spaced name.txt ; size=3").1,
            s("spaced name.txt")
        );
    }

    #[test]
    fn rejected_extended_values() {
        assert_eq!(f("attachment; filename*=Shift_JIS''%82%A0").0, None);
        assert_eq!(f("attachment; filename*=UTF-8''%FF%FE").0, None);
        assert_eq!(f("attachment; filename*=UTF-8''%zz").0, None);
        assert_eq!(f("attachment; filename*=no-quotes").0, None);
    }

    #[test]
    fn no_params() {
        assert_eq!(f("attachment"), (None, None));
        assert_eq!(f(""), (None, None));
        assert_eq!(f("attachment; size=10; filename"), (None, None));
    }
}
