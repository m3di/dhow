//! One download, from probe to a fully committed part file: resume or
//! reset, then fill every gap (ENGINE-SPEC §2, §5.4, §6 I3–I5, I7, §7, §9).
//! Digest check and the final rename are the completion gate's (§8).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use reqwest::{Client, Url};

use crate::http::Header;
use crate::journal::{Header as JournalHeader, Journal, JournalError, format};
use crate::probe::{self, ProbeError, ProbeInfo, ProbeOptions, Resumable};
use crate::rangeset::RangeSet;
use crate::transfer::{self, ConnEvent, OVERLAP, Overlap, Source, TransferConfig, TransferError};
use crate::writer::actor::{self, Outcome, WriterGone};
use crate::writer::{self, Writer, WriterConfig, WriterError};

pub struct Request {
    pub url: Url,
    pub headers: Vec<Header>,
    pub part: PathBuf,
    pub sidecar: PathBuf,
}

#[derive(Clone, Copy, Debug)]
pub struct DownloadConfig {
    pub probe: ProbeOptions,
    pub transfer: TransferConfig,
    pub writer: WriterConfig,
    /// Restarts from zero allowed in one run before giving up, so a server
    /// whose file changes on every request can't loop forever.
    pub max_resets: u32,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            probe: ProbeOptions::default(),
            transfer: TransferConfig::default(),
            writer: WriterConfig::default(),
            max_resets: 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResetReason {
    /// I4/I5: the server has a different file now ("The file on the
    /// server changed; starting over").
    ServerFileChanged,
    /// A part file without a usable sidecar, or the reverse (§5.4).
    Unaccountable,
    /// Bytes are on disk but the server can't continue from them.
    CannotResume,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DownloadEvent {
    Probed(Box<ProbeInfo>),
    /// Continuing a journal that already has `committed` durable bytes.
    Resumed {
        committed: u64,
    },
    /// Starting over from zero; everything on disk was discarded.
    Reset(ResetReason),
    Connection(ConnEvent),
}

#[derive(Debug)]
pub struct Downloaded {
    pub info: ProbeInfo,
    pub committed: RangeSet,
    /// Blind mode ended at EOF with no `Content-Length` to check (I7).
    pub size_unverified: bool,
    /// No `If-Range` validator: only overlap checks guarded resume (I4).
    pub identity_weak: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error(transparent)]
    Probe(#[from] ProbeError),
    #[error(transparent)]
    Transfer(#[from] TransferError),
    #[error(transparent)]
    Writer(#[from] WriterError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("the file on the server kept changing ({0} restarts)")]
    TooManyResets(u32),
}

enum Stop {
    Reset(ResetReason),
    Failed(DownloadError),
}

impl<E: Into<DownloadError>> From<E> for Stop {
    fn from(e: E) -> Self {
        Stop::Failed(e.into())
    }
}

pub async fn download(
    req: &Request,
    client: &Client,
    config: &DownloadConfig,
    on_event: &(dyn Fn(DownloadEvent) + Send + Sync),
) -> Result<Downloaded, DownloadError> {
    let mut resets = 0;
    let mut force = None;
    loop {
        // Re-probe every start: redirects re-resolve, and the identity to
        // resume against is checked against the server's current one.
        let info = probe::probe(client, &req.url, &req.headers, config.probe).await?;
        on_event(DownloadEvent::Probed(Box::new(info.clone())));
        let (file, journal) = prepare(req, &info, force.take(), on_event)?;
        match fetch(req, client, config, &info, file, journal, on_event).await {
            Ok(done) => return Ok(done),
            Err(Stop::Failed(e)) => return Err(e),
            Err(Stop::Reset(reason)) => {
                if resets == config.max_resets {
                    return Err(DownloadError::TooManyResets(resets));
                }
                resets += 1;
                force = Some(reason);
            }
        }
    }
}

/// Opens the journal and part file to resume, or discards both and starts
/// fresh, emitting `Resumed` or `Reset` accordingly.
fn prepare(
    req: &Request,
    info: &ProbeInfo,
    force: Option<ResetReason>,
    on_event: &(dyn Fn(DownloadEvent) + Send + Sync),
) -> Result<(File, Journal), DownloadError> {
    let reason = match force {
        Some(reason) => Some(reason),
        None => match resumable_state(req, info)? {
            Ok((file, journal)) => {
                on_event(DownloadEvent::Resumed {
                    committed: journal.committed().len(),
                });
                return Ok((file, journal));
            }
            Err(reason) => reason,
        },
    };
    // Sidecar first: a crash in between leaves a part without a sidecar,
    // which is discarded on the next start (§5.4), never trusted.
    remove_if_exists(&req.sidecar)?;
    remove_if_exists(&req.part)?;
    let (file, preallocated) = writer::create_part(&req.part, info.total_size)?;
    let header = JournalHeader {
        preallocated,
        total_size: info.total_size,
        url_hash: format::url_hash(req.url.as_str()),
        etag: info.etag.clone(),
        last_modified: info.last_modified.as_deref().and_then(unix_secs),
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64),
    };
    let journal = Journal::create(&req.sidecar, header)?;
    if let Some(reason) = reason {
        tracing::info!(?reason, "download: starting over");
        on_event(DownloadEvent::Reset(reason));
    }
    Ok((file, journal))
}

/// `Ok(Ok(..))`: resume from these. `Ok(Err(None))`: nothing on disk.
/// `Ok(Err(Some(reason)))`: what's on disk can't be used.
#[allow(clippy::type_complexity)]
fn resumable_state(
    req: &Request,
    info: &ProbeInfo,
) -> Result<Result<(File, Journal), Option<ResetReason>>, DownloadError> {
    let part_exists = req.part.try_exists()?;
    let journal = match Journal::open(&req.sidecar) {
        Ok(j) => Some(j),
        Err(JournalError::Io(e)) if e.kind() == io::ErrorKind::NotFound => None,
        Err(JournalError::Header(e)) => {
            tracing::warn!(error = %e, "download: unusable sidecar");
            return Ok(Err(Some(ResetReason::Unaccountable)));
        }
        Err(JournalError::Io(e)) => return Err(e.into()),
    };
    let journal = match (part_exists, journal) {
        (false, None) => return Ok(Err(None)),
        (true, Some(j)) => j,
        _ => return Ok(Err(Some(ResetReason::Unaccountable))),
    };
    if !same_identity(journal.header(), req, info) {
        return Ok(Err(Some(ResetReason::ServerFileChanged)));
    }
    let can_range = info.resumable != Resumable::No && info.total_size.is_some();
    if !journal.committed().is_empty() && !can_range {
        return Ok(Err(Some(ResetReason::CannotResume)));
    }
    let file = OpenOptions::new().read(true).write(true).open(&req.part)?;
    match info.total_size {
        // Created at full size (preallocated or sparse); anything else
        // isn't the file the journal describes.
        Some(total) if file.metadata()?.len() != total => {
            return Ok(Err(Some(ResetReason::Unaccountable)));
        }
        Some(_) => {}
        // Blind: nothing committed (checked above), so drop whatever
        // uncommitted bytes are there; a stale tail would end up in the file.
        None => file.set_len(0)?,
    }
    Ok(Ok((file, journal)))
}

/// I4: the sidecar's identity still describes what the server has. With
/// neither ETag nor Last-Modified recorded, overlap verification (I5) is
/// the only check, and it happens on the first resumed request.
fn same_identity(h: &JournalHeader, req: &Request, info: &ProbeInfo) -> bool {
    h.url_hash == format::url_hash(req.url.as_str())
        && h.total_size == info.total_size
        && match (&h.etag, h.last_modified) {
            (Some(etag), _) => info.etag.as_ref() == Some(etag),
            (None, Some(lm)) => info.last_modified.as_deref().and_then(unix_secs) == Some(lm),
            (None, None) => true,
        }
}

async fn fetch(
    req: &Request,
    client: &Client,
    config: &DownloadConfig,
    info: &ProbeInfo,
    file: File,
    journal: Journal,
    on_event: &(dyn Fn(DownloadEvent) + Send + Sync),
) -> Result<Downloaded, Stop> {
    let total = info.total_size;
    let committed = journal.committed().clone();
    let gaps = match total {
        Some(t) => committed.gaps(t),
        None => RangeSet::new().gaps(u64::MAX),
    };
    // Overlaps are read before the writer owns the file (I9).
    let mut plan = Vec::with_capacity(gaps.len());
    for gap in gaps {
        let overlap = match committed.ending_at(gap.start) {
            Some(prev) => {
                let start = prev.start.max(gap.start.saturating_sub(OVERLAP));
                let expected = writer::read_at(&file, start, (gap.start - start) as usize)?;
                Some(Overlap {
                    start,
                    expected: Bytes::from(expected),
                })
            }
            None => None,
        };
        plan.push((gap, overlap));
    }

    let validator = transfer::validator(info.etag.as_deref(), info.last_modified.as_deref());
    let src = Source {
        client: client.clone(),
        url: req.url.clone(),
        headers: req.headers.clone(),
        total,
        resumable: info.resumable,
        validator: validator.clone(),
    };
    let (handle, done) = actor::spawn(Writer::new(file, journal, config.writer))?;
    let conn_event = |e: ConnEvent| on_event(DownloadEvent::Connection(e));
    let mut content_length_seen = true;
    for (id, (range, overlap)) in (0u32..).zip(plan) {
        if handle.define(id, range.clone()).await.is_err() {
            return Err(writer_failure(done).await.into());
        }
        let result = transfer::run_connection(
            &src,
            id,
            range,
            overlap,
            &handle,
            &config.transfer,
            &conn_event,
        )
        .await;
        match result {
            Ok(finished) => content_length_seen = finished.content_length_seen,
            Err(e) => {
                // Let the writer commit what arrived, and stop, before
                // anything touches the files again.
                drop(handle);
                let writer_result = done.await;
                return Err(match (e, writer_result) {
                    (_, Ok(Err(we))) => Stop::Failed(we.into()),
                    (TransferError::IdentityChanged(change), _) => {
                        tracing::warn!(?change, "download: identity changed");
                        Stop::Reset(ResetReason::ServerFileChanged)
                    }
                    (TransferError::NeedsRestart, _) => Stop::Reset(ResetReason::CannotResume),
                    (e, _) => Stop::Failed(e.into()),
                });
            }
        }
    }
    let committed = match handle.finish().await {
        Ok(c) => c,
        Err(WriterGone) => return Err(writer_failure(done).await.into()),
    };
    // I7: exactly [0, total). Blind mode has no total to check.
    if let Some(t) = total
        && !committed.is_exactly(t)
    {
        let msg = format!("committed {committed:?} is not exactly [0, {t})");
        return Err(io::Error::other(msg).into());
    }
    Ok(Downloaded {
        info: info.clone(),
        committed,
        size_unverified: total.is_none() && !content_length_seen,
        identity_weak: validator.is_none(),
    })
}

async fn writer_failure(
    done: tokio::sync::oneshot::Receiver<Result<Outcome, WriterError>>,
) -> DownloadError {
    match done.await {
        Ok(Err(e)) => e.into(),
        _ => io::Error::other("writer stopped unexpectedly").into(),
    }
}

fn unix_secs(http_date: &str) -> Option<i64> {
    let t = httpdate::parse_http_date(http_date).ok()?;
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}
