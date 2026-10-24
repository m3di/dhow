//! Server-offered digests for I8 (ENGINE-SPEC §6).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::HeaderMap;

/// Ordered weakest to strongest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DigestAlgorithm {
    /// Verify only.
    Md5,
    /// Verify only.
    Sha1,
    Sha256,
    Sha512,
}

impl DigestAlgorithm {
    fn len(self) -> usize {
        match self {
            Self::Md5 => 16,
            Self::Sha1 => 20,
            Self::Sha256 => 32,
            Self::Sha512 => 64,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest {
    pub algorithm: DigestAlgorithm,
    pub value: Vec<u8>,
}

/// The strongest whole-file digest in `headers`. On a partial (206) response
/// `Content-Digest` and `Content-MD5` describe only the part, so only
/// `Repr-Digest` (RFC 9530) and `Digest` (RFC 3230, instance digest) count.
pub(crate) fn from_headers(headers: &HeaderMap, partial: bool) -> Option<Digest> {
    let values = |name: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .collect::<Vec<_>>()
    };
    let mut found = Vec::new();
    let mut fields = vec!["repr-digest"];
    if !partial {
        fields.push("content-digest");
    }
    for field in fields {
        for member in values(field) {
            // RFC 8941 dictionary member: `sha-256=:base64:` (parameters ignored).
            let Some((alg, rest)) = member.split_once('=') else {
                continue;
            };
            let rest = rest.split(';').next().unwrap_or_default();
            let Some(b64) = rest.strip_prefix(':').and_then(|r| r.strip_suffix(':')) else {
                continue;
            };
            found.extend(algorithm(alg).and_then(|a| decode(a, b64)));
        }
    }
    for member in values("digest") {
        if let Some((alg, b64)) = member.split_once('=') {
            found.extend(algorithm(alg).and_then(|a| decode(a, b64)));
        }
    }
    if !partial {
        for b64 in values("content-md5") {
            found.extend(decode(DigestAlgorithm::Md5, b64));
        }
    }
    found.into_iter().max_by_key(|d| d.algorithm)
}

fn algorithm(name: &str) -> Option<DigestAlgorithm> {
    match name.trim().to_ascii_lowercase().as_str() {
        "md5" => Some(DigestAlgorithm::Md5),
        "sha" | "sha-1" => Some(DigestAlgorithm::Sha1),
        "sha-256" => Some(DigestAlgorithm::Sha256),
        "sha-512" => Some(DigestAlgorithm::Sha512),
        _ => None,
    }
}

fn decode(algorithm: DigestAlgorithm, b64: &str) -> Option<Digest> {
    let value = STANDARD.decode(b64.trim()).ok()?;
    (value.len() == algorithm.len()).then_some(Digest { algorithm, value })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(len: usize, byte: u8) -> String {
        STANDARD.encode(vec![byte; len])
    }

    fn headers(pairs: &[(&'static str, String)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, v.parse().unwrap());
        }
        h
    }

    fn alg(h: &HeaderMap, partial: bool) -> Option<DigestAlgorithm> {
        from_headers(h, partial).map(|d| d.algorithm)
    }

    #[test]
    fn content_digest_dictionary() {
        let h = headers(&[(
            "content-digest",
            format!("unixsum=:AAA=:, sha-256=:{}:", b64(32, 7)),
        )]);
        let d = from_headers(&h, false).unwrap();
        assert_eq!(d.algorithm, DigestAlgorithm::Sha256);
        assert_eq!(d.value, vec![7; 32]);
    }

    #[test]
    fn rfc3230_digest_and_content_md5() {
        let h = headers(&[("digest", format!("SHA={}", b64(20, 1)))]);
        assert_eq!(alg(&h, false), Some(DigestAlgorithm::Sha1));
        let h = headers(&[("content-md5", b64(16, 1))]);
        assert_eq!(alg(&h, false), Some(DigestAlgorithm::Md5));
    }

    #[test]
    fn strongest_wins_across_fields() {
        let h = headers(&[
            ("content-md5", b64(16, 1)),
            ("digest", format!("sha-256={}", b64(32, 1))),
            ("content-digest", format!("sha-512=:{}:", b64(64, 1))),
        ]);
        assert_eq!(alg(&h, false), Some(DigestAlgorithm::Sha512));
    }

    #[test]
    fn partial_response_ignores_part_digests() {
        let h = headers(&[
            ("content-digest", format!("sha-512=:{}:", b64(64, 1))),
            ("content-md5", b64(16, 1)),
        ]);
        assert_eq!(alg(&h, true), None);
        let h = headers(&[("repr-digest", format!("sha-256=:{}:", b64(32, 1)))]);
        assert_eq!(alg(&h, true), Some(DigestAlgorithm::Sha256));
    }

    #[test]
    fn wrong_length_or_garbage_is_ignored() {
        let h = headers(&[
            ("content-digest", format!("sha-256=:{}:", b64(31, 1))),
            ("content-digest", "sha-256=:!!!:".to_owned()),
            ("content-digest", format!("sha-256={}", b64(32, 1))),
        ]);
        assert_eq!(alg(&h, false), None);
    }
}
