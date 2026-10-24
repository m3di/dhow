//! One connection's transfer loop (ENGINE-SPEC §7) with retries (§9).
//!
//! [`run_connection`] fills one chunk through a [`WriterHandle`]: range
//! request with `If-Range`, response validation (I4), bounded writes (I6),
//! reconnect from the current position on failure. It reports, rather than
//! acts on, a changed file ([`TransferError::IdentityChanged`]) and a lost
//! non-resumable connection ([`TransferError::NeedsRestart`]): resetting the
//! download belongs to the coordinator (M1.6).

pub mod retry;

use std::ops::Range;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use reqwest::header::{CONTENT_LENGTH, ETAG, IF_RANGE, LAST_MODIFIED, RANGE, RETRY_AFTER};
use reqwest::{Client, Response, Url};

pub use retry::RetryPolicy;

use crate::http::{self, Header};
use crate::probe::{Resumable, content_length, content_range};
use crate::writer::actor::{WriterGone, WriterHandle};
use crate::writer::{self, ChunkId, Overflow};

/// What is being downloaded, as the probe established it.
#[derive(Clone, Debug)]
pub struct Source {
    pub client: Client,
    /// The *original* URL: redirects are followed on every request because
    /// signed redirect targets expire mid-download.
    pub url: Url,
    pub headers: Vec<Header>,
    pub total: Option<u64>,
    pub resumable: Resumable,
    /// `If-Range` value, see [`validator`].
    pub validator: Option<String>,
}

/// `If-Range` value (I4): a strong ETag, else `Last-Modified`, else none
/// (resume then relies on overlap verification alone).
pub fn validator(etag: Option<&str>, last_modified: Option<&str>) -> Option<String> {
    etag.filter(|e| !e.starts_with("W/"))
        .or(last_modified)
        .map(str::to_owned)
}

#[derive(Clone, Copy, Debug)]
pub struct TransferConfig {
    /// From sending the request to response headers.
    pub first_byte: Duration,
    /// Longest silence while reading the body.
    pub idle: Duration,
    pub retry: RetryPolicy,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            first_byte: Duration::from_secs(30),
            idle: Duration::from_secs(30),
            retry: RetryPolicy::default(),
        }
    }
}

/// How far back a resumed request starts (I5).
pub const OVERLAP: u64 = 64 * 1024;

/// Committed bytes `[start, start + expected.len())` that end exactly where
/// a resumed chunk begins. The server must send them again, byte-equal,
/// before anything new is written (I5).
#[derive(Clone, Debug)]
pub struct Overlap {
    pub start: u64,
    pub expected: Bytes,
}

/// Why an attempt failed in a way worth retrying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cause {
    Status(u16),
    Timeout,
    Connect,
    Network,
    /// The body ended before the bytes the response promised.
    Truncated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnEvent {
    /// Bytes handed to the writer.
    Received(u64),
    /// About to wait `delay` and reconnect ("reconnecting" in the UI).
    Retrying {
        failures: u32,
        delay: Duration,
        cause: Cause,
    },
    /// The server answered a range request with the whole file at offset 0:
    /// the download continues as non-resumable (§2.3 `Unknown` downgrade).
    NotResumable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityChange {
    /// `200` to a range request past offset 0.
    RangeIgnored,
    /// `200` to a range request whose ETag / Last-Modified no longer match
    /// the `If-Range` validator: the file was replaced.
    ValidatorMismatch,
    /// `Content-Range` total, or a full response's length, differs.
    TotalMismatch { expected: u64, got: Option<u64> },
    /// `Content-Range` missing, malformed, or not what was asked for.
    BadContentRange,
    /// `416` to a range inside the probed size.
    Unsatisfiable,
    /// The re-sent overlap differs from what is on disk (I5).
    OverlapMismatch,
}

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("gave up after {failures} consecutive failures (last: {last:?})")]
    Transient { failures: u32, last: Cause },
    #[error("server answered {0}")]
    Permanent(u16),
    #[error("more than {} redirects", http::MAX_REDIRECTS)]
    TooManyRedirects,
    #[error("invalid request header {0:?}")]
    InvalidHeader(String),
    #[error("the file on the server changed: {0:?}")]
    IdentityChanged(IdentityChange),
    #[error("connection lost on a server that cannot resume")]
    NeedsRestart,
    #[error("server sent more than the file's size")]
    ServerSentExtraData,
    #[error(transparent)]
    Writer(#[from] WriterGone),
}

/// The chunk is filled (or, in blind mode, the body ended).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Finished {
    /// Whether the final response carried `Content-Length`. Blind-mode
    /// completion requires that it didn't (I7).
    pub content_length_seen: bool,
}

enum Fail {
    Fatal(TransferError),
    Retry {
        cause: Cause,
        retry_after: Option<Duration>,
    },
    /// The server sent less than asked but all it promised (a capped
    /// range): reconnect at once, not a failure.
    Again,
}

impl From<TransferError> for Fail {
    fn from(e: TransferError) -> Self {
        Fail::Fatal(e)
    }
}

fn retry(cause: Cause) -> Fail {
    Fail::Retry {
        cause,
        retry_after: None,
    }
}

fn identity(change: IdentityChange) -> Fail {
    Fail::Fatal(TransferError::IdentityChanged(change))
}

/// Fills `range` of chunk `chunk`. Retries per §9: transient failures back
/// off and reconnect from the current position; `max_failures` consecutive
/// failures (an attempt that wrote bytes resets the count) give up.
pub async fn run_connection(
    src: &Source,
    chunk: ChunkId,
    range: Range<u64>,
    overlap: Option<Overlap>,
    writer: &WriterHandle,
    config: &TransferConfig,
    on_event: &(dyn Fn(ConnEvent) + Send + Sync),
) -> Result<Finished, TransferError> {
    let mut pos = range.start;
    let mut resumable = src.resumable;
    let mut failures = 0u32;
    let mut verify = overlap.filter(|o| !o.expected.is_empty());
    let verified = |v: &Option<Overlap>| v.as_ref().map_or(u64::MAX, |o| o.start);
    loop {
        let before = (pos, verified(&verify));
        let fail = match attempt(
            src,
            &mut resumable,
            chunk,
            &mut pos,
            &mut verify,
            &range,
            writer,
            config,
            on_event,
        )
        .await
        {
            Ok(done) => return Ok(done),
            Err(fail) => fail,
        };
        let (cause, retry_after) = match fail {
            Fail::Fatal(e) => return Err(e),
            Fail::Again => continue,
            Fail::Retry { cause, retry_after } => (cause, retry_after),
        };
        let can_resume = resumable != Resumable::No && src.total.is_some();
        if pos > range.start && !can_resume {
            return Err(TransferError::NeedsRestart);
        }
        if pos > before.0 || verified(&verify) > before.1 {
            failures = 0;
        }
        failures += 1;
        if failures >= config.retry.max_failures {
            return Err(TransferError::Transient {
                failures,
                last: cause,
            });
        }
        let delay = match retry_after {
            Some(d) => d.min(config.retry.retry_after_cap),
            None => config.retry.backoff(failures, fastrand::f64()),
        };
        tracing::debug!(chunk, failures, ?delay, ?cause, "transfer: retrying");
        on_event(ConnEvent::Retrying {
            failures,
            delay,
            cause,
        });
        tokio::time::sleep(delay).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn attempt(
    src: &Source,
    resumable: &mut Resumable,
    chunk: ChunkId,
    pos: &mut u64,
    verify: &mut Option<Overlap>,
    range: &Range<u64>,
    writer: &WriterHandle,
    config: &TransferConfig,
    on_event: &(dyn Fn(ConnEvent) + Send + Sync),
) -> Result<Finished, Fail> {
    let ranged = *resumable != Resumable::No && src.total.is_some();
    let req_start = verify.as_ref().map_or(*pos, |v| v.start);
    let mut req = http::with_headers(src.client.get(src.url.clone()), &src.headers)
        .map_err(TransferError::InvalidHeader)?;
    if ranged {
        req = req.header(RANGE, format!("bytes={req_start}-{}", range.end - 1));
        if let Some(v) = &src.validator {
            req = req.header(IF_RANGE, v.as_str());
        }
    }
    let mut resp = match tokio::time::timeout(config.first_byte, req.send()).await {
        Err(_) => return Err(retry(Cause::Timeout)),
        Ok(Err(e)) => return Err(classify(&e)),
        Ok(Ok(r)) => r,
    };
    let resp_end = accept(src, resumable, req_start, range, &resp, ranged, on_event)?;
    let content_length_seen = resp.headers().contains_key(CONTENT_LENGTH);
    // Read to EOF when the chunk ends the file, so bytes past total_size are
    // seen (I6); otherwise stop at the chunk's end.
    let ends_file = src.total == Some(range.end);

    loop {
        let next = match tokio::time::timeout(config.idle, resp.chunk()).await {
            Err(_) => return Err(retry(Cause::Timeout)),
            Ok(Err(e)) => return Err(classify(&e)),
            Ok(Ok(next)) => next,
        };
        let Some(mut bytes) = next else { break };
        if let Some(v) = verify.as_mut() {
            let n = v.expected.len().min(bytes.len());
            if bytes[..n] != v.expected[..n] {
                return Err(identity(IdentityChange::OverlapMismatch));
            }
            v.expected = v.expected.slice(n..);
            v.start += n as u64;
            bytes = bytes.slice(n..);
            if v.expected.is_empty() {
                *verify = None;
            }
            if bytes.is_empty() {
                continue;
            }
        }
        let (keep, overflow) = writer::bound(*pos, range.end, src.total, bytes.len());
        if keep > 0 {
            writer
                .write(chunk, *pos, bytes.slice(..keep))
                .await
                .map_err(TransferError::from)?;
            *pos += keep as u64;
            on_event(ConnEvent::Received(keep as u64));
        }
        match overflow {
            Overflow::None => {}
            // Dropping the response closes the connection (I6).
            Overflow::PastChunk => {
                return Ok(Finished {
                    content_length_seen,
                });
            }
            Overflow::PastTotal => return Err(TransferError::ServerSentExtraData.into()),
        }
        if *pos == range.end && !ends_file {
            return Ok(Finished {
                content_length_seen,
            });
        }
    }

    match src.total {
        None => Ok(Finished {
            content_length_seen,
        }),
        Some(_) if *pos == range.end => Ok(Finished {
            content_length_seen,
        }),
        Some(_) if verify.as_ref().map_or(*pos, |v| v.start) < resp_end => {
            Err(retry(Cause::Truncated))
        }
        Some(_) => Err(Fail::Again),
    }
}

/// Validates the response status and range (I4). Returns where this
/// response's bytes end.
fn accept(
    src: &Source,
    resumable: &mut Resumable,
    req_start: u64,
    range: &Range<u64>,
    resp: &Response,
    ranged: bool,
    on_event: &(dyn Fn(ConnEvent) + Send + Sync),
) -> Result<u64, Fail> {
    let status = resp.status().as_u16();
    let headers = resp.headers();
    match status {
        206 if ranged => {
            let Some((first, last, total)) = content_range(headers) else {
                return Err(identity(IdentityChange::BadContentRange));
            };
            if total != src.total {
                let expected = src.total.unwrap_or_default();
                return Err(identity(IdentityChange::TotalMismatch {
                    expected,
                    got: total,
                }));
            }
            // A server may cap a range; a prefix of what we asked is fine.
            if first != req_start || last < first || last >= range.end {
                return Err(identity(IdentityChange::BadContentRange));
            }
            Ok(last + 1)
        }
        200 => {
            // A full body is usable only if the request started at offset
            // 0 (then any overlap is verified against it as usual).
            if req_start != 0 {
                return Err(identity(IdentityChange::RangeIgnored));
            }
            if let (Some(expected), Some(got)) = (src.total, content_length(headers)) {
                // Longer is caught as extra data while streaming (I6).
                if got < expected {
                    return Err(identity(IdentityChange::TotalMismatch {
                        expected,
                        got: Some(got),
                    }));
                }
            }
            if ranged {
                // If-Range failed or the server ignores ranges. Only the
                // latter is a downgrade; the former is a different file.
                let current = |name| headers.get(name).and_then(|v| v.to_str().ok());
                if let Some(v) = &src.validator {
                    let still = [current(ETAG), current(LAST_MODIFIED)].contains(&Some(v.as_str()));
                    if !still {
                        return Err(identity(IdentityChange::ValidatorMismatch));
                    }
                }
                *resumable = Resumable::No;
                on_event(ConnEvent::NotResumable);
            }
            Ok(range.end)
        }
        416 => Err(identity(IdentityChange::Unsatisfiable)),
        408 | 429 | 500..=599 => Err(Fail::Retry {
            cause: Cause::Status(status),
            retry_after: headers
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| retry::retry_after(v, SystemTime::now())),
        }),
        _ => Err(TransferError::Permanent(status).into()),
    }
}

fn classify(e: &reqwest::Error) -> Fail {
    if e.is_redirect() {
        Fail::Fatal(TransferError::TooManyRedirects)
    } else if e.is_timeout() {
        retry(Cause::Timeout)
    } else if e.is_connect() {
        retry(Cause::Connect)
    } else {
        retry(Cause::Network)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validator_prefers_strong_etag() {
        assert_eq!(
            validator(Some("\"a\""), Some("date")).as_deref(),
            Some("\"a\"")
        );
        assert_eq!(
            validator(Some("W/\"a\""), Some("date")).as_deref(),
            Some("date")
        );
        assert_eq!(validator(Some("W/\"a\""), None), None);
        assert_eq!(validator(None, None), None);
    }
}
