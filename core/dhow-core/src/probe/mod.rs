//! Probe (ENGINE-SPEC §2): learn size, resumability and identity before any
//! byte is written. Response bodies are never read; the 0-0 `GET`'s body is
//! dropped with the response.

mod digest;

pub use digest::{Digest, DigestAlgorithm};

use reqwest::header::{self, HeaderMap};
use reqwest::{Client, Method, StatusCode, Url};

use crate::http::{self, Header, redact};
use crate::naming::{self, TargetFs};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resumable {
    Yes,
    No,
    /// Planned as `Yes`, downgraded on the first `200` to a `Range`.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeInfo {
    /// After redirects, from the response that settled the probe.
    pub effective_url: Url,
    pub status: u16,
    pub total_size: Option<u64>,
    pub resumable: Resumable,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub content_disposition: Option<String>,
    pub digest: Option<Digest>,
    /// [`naming::base_name`] for the host filesystem. Collisions are resolved
    /// at `Add` (and again at rename) against the real target directory.
    pub suggested_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProbeError {
    #[error("unsupported URL scheme {0:?}")]
    UnsupportedScheme(String),
    #[error("invalid request header {0:?}")]
    InvalidHeader(String),
    #[error("server answered {0}")]
    Status(u16),
    #[error("more than {} redirects", http::MAX_REDIRECTS)]
    TooManyRedirects,
    #[error("timed out")]
    Timeout,
    #[error("could not connect")]
    Connect,
    #[error("network error")]
    Network,
}

#[derive(Clone, Copy, Debug)]
pub struct ProbeOptions {
    /// Send `GET` with `Range: bytes=0-0` when `HEAD` doesn't settle size and
    /// resumability. Users turn this off for links that count every `GET`.
    pub range_get: bool,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self { range_get: true }
    }
}

/// What a response said. Its body is never read.
struct Seen {
    status: StatusCode,
    url: Url,
    headers: HeaderMap,
}

pub async fn probe(
    client: &Client,
    url: &Url,
    headers: &[Header],
    opts: ProbeOptions,
) -> Result<ProbeInfo, ProbeError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ProbeError::UnsupportedScheme(url.scheme().to_owned()));
    }
    let target = redact(url);

    // Any HEAD failure falls through to the GET: plenty of servers reject HEAD
    // with something other than 405/501 (e.g. 403 from URLs signed for GET).
    let (head, head_err) = match send(client, Method::HEAD, url, headers).await {
        Ok(r) if r.status.is_success() => (Some(r), None),
        Ok(r) => {
            tracing::debug!(url = %target, status = r.status.as_u16(), "probe: HEAD rejected");
            (None, Some(ProbeError::Status(r.status.as_u16())))
        }
        Err(e @ ProbeError::InvalidHeader(_)) => return Err(e),
        Err(e) => {
            tracing::debug!(url = %target, error = %e, "probe: HEAD failed");
            (None, Some(e))
        }
    };
    let head_settles = head.as_ref().is_some_and(|h| {
        content_length(&h.headers).is_some() && accept_ranges(&h.headers).is_some()
    });
    if head_settles || !opts.range_get {
        return match head {
            Some(h) => Ok(info(url, Some(&h), None)),
            None => Err(head_err.unwrap_or(ProbeError::Network)),
        };
    }

    match send(client, Method::GET, url, headers).await {
        Ok(g) if matches!(g.status.as_u16(), 200 | 206 | 416) => {
            Ok(info(url, head.as_ref(), Some(&g)))
        }
        result => {
            let err = match result {
                Ok(g) => ProbeError::Status(g.status.as_u16()),
                Err(e) => e,
            };
            tracing::debug!(url = %target, error = %err, "probe: range GET failed");
            match head {
                Some(h) => Ok(info(url, Some(&h), None)),
                None => Err(err),
            }
        }
    }
}

async fn send(
    client: &Client,
    method: Method,
    url: &Url,
    headers: &[Header],
) -> Result<Seen, ProbeError> {
    let is_get = method == Method::GET;
    let mut req = http::with_headers(client.request(method, url.clone()), headers)
        .map_err(ProbeError::InvalidHeader)?;
    if is_get {
        req = req.header(header::RANGE, "bytes=0-0");
    }
    let resp = req.send().await.map_err(classify)?;
    Ok(Seen {
        status: resp.status(),
        url: resp.url().clone(),
        headers: resp.headers().clone(),
    })
}

fn classify(e: reqwest::Error) -> ProbeError {
    if e.is_redirect() {
        ProbeError::TooManyRedirects
    } else if e.is_timeout() {
        ProbeError::Timeout
    } else if e.is_connect() {
        ProbeError::Connect
    } else {
        // Without the URL: it may carry credentials.
        tracing::debug!(error = %e.without_url(), "probe: network error");
        ProbeError::Network
    }
}

/// Combines what `HEAD` and the 0-0 `GET` said (ENGINE-SPEC §2.2, §2.3). The
/// `GET`, when present, is the more direct evidence and wins.
fn info(original_url: &Url, head: Option<&Seen>, get: Option<&Seen>) -> ProbeInfo {
    let last = get.or(head).expect("at least one response");
    let head_length = head.and_then(|h| content_length(&h.headers));
    let head_ranges = head.and_then(|h| accept_ranges(&h.headers));

    let (total_size, resumable) = match get {
        Some(g) if g.status == StatusCode::PARTIAL_CONTENT => match content_range(&g.headers) {
            Some((0, 0, total)) => (total.or(head_length), Resumable::Yes),
            // A 206 that isn't what we asked for: don't trust its ranges.
            _ => (head_length, Resumable::No),
        },
        // 0-0 unsatisfiable: an empty file (`bytes */0`), or a confused server.
        Some(g) if g.status == StatusCode::RANGE_NOT_SATISFIABLE => {
            let total = unsatisfied_total(&g.headers).or(head_length);
            (
                total,
                if total == Some(0) {
                    Resumable::Yes
                } else {
                    Resumable::Unknown
                },
            )
        }
        Some(g) => (content_length(&g.headers).or(head_length), Resumable::No),
        None => {
            let resumable = match head_ranges {
                Some(false) => Resumable::No,
                Some(true) if head_length.is_some() => Resumable::Yes,
                _ => Resumable::Unknown,
            };
            (head_length, resumable)
        }
    };

    // Raw bytes, not `to_str()`: servers routinely send UTF-8 (or Latin-1)
    // filenames in Content-Disposition.
    let text = |name: header::HeaderName| {
        let read = |s: &Seen| {
            s.headers
                .get(&name)
                .map(|v| naming::decode_text(v.as_bytes().to_vec()))
        };
        get.and_then(read).or_else(|| head.and_then(read))
    };
    let digest = [
        head.and_then(|h| digest::from_headers(&h.headers, false)),
        get.and_then(|g| digest::from_headers(&g.headers, g.status == StatusCode::PARTIAL_CONTENT)),
    ]
    .into_iter()
    .flatten()
    .max_by_key(|d| d.algorithm);

    let content_type = text(header::CONTENT_TYPE);
    let content_disposition = text(header::CONTENT_DISPOSITION);
    let suggested_name = naming::base_name(
        content_disposition.as_deref(),
        content_type.as_deref(),
        &last.url,
        original_url,
        TargetFs::host(),
    );
    ProbeInfo {
        effective_url: last.url.clone(),
        status: last.status.as_u16(),
        total_size,
        resumable,
        etag: text(header::ETAG),
        last_modified: text(header::LAST_MODIFIED),
        content_type,
        content_disposition,
        digest,
        suggested_name,
    }
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, header::CONTENT_LENGTH)?
        .trim()
        .parse()
        .ok()
}

/// `Some(true)` for `bytes`, `Some(false)` for `none`, `None` if absent or other.
fn accept_ranges(headers: &HeaderMap) -> Option<bool> {
    let value = header_str(headers, header::ACCEPT_RANGES)?;
    let mut units = value.split(',').map(|u| u.trim().to_ascii_lowercase());
    if units.clone().any(|u| u == "bytes") {
        Some(true)
    } else if units.any(|u| u == "none") {
        Some(false)
    } else {
        None
    }
}

/// `bytes <first>-<last>/<total|*>` → `(first, last, total)`.
fn content_range(headers: &HeaderMap) -> Option<(u64, u64, Option<u64>)> {
    let spec = header_str(headers, header::CONTENT_RANGE)?
        .trim()
        .strip_prefix("bytes ")?;
    let (range, total) = spec.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    let total = match total {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    Some((first.parse().ok()?, last.parse().ok()?, total))
}

/// `bytes */<total>` on a 416.
fn unsatisfied_total(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, header::CONTENT_RANGE)?
        .trim()
        .strip_prefix("bytes */")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.append(*k, v.parse().unwrap());
        }
        m
    }

    #[test]
    fn parses_content_range() {
        assert_eq!(
            content_range(&h(&[("content-range", "bytes 0-0/1234")])),
            Some((0, 0, Some(1234)))
        );
        assert_eq!(
            content_range(&h(&[("content-range", "bytes 0-0/*")])),
            Some((0, 0, None))
        );
        assert_eq!(
            content_range(&h(&[("content-range", "bytes */1234")])),
            None
        );
        assert_eq!(content_range(&h(&[("content-range", "items 0-0/1")])), None);
        assert_eq!(
            unsatisfied_total(&h(&[("content-range", "bytes */0")])),
            Some(0)
        );
    }

    #[test]
    fn parses_accept_ranges() {
        assert_eq!(accept_ranges(&h(&[("accept-ranges", "bytes")])), Some(true));
        assert_eq!(accept_ranges(&h(&[("accept-ranges", "None")])), Some(false));
        assert_eq!(
            accept_ranges(&h(&[("accept-ranges", "items, bytes")])),
            Some(true)
        );
        assert_eq!(accept_ranges(&h(&[("accept-ranges", "items")])), None);
        assert_eq!(accept_ranges(&h(&[])), None);
    }
}
