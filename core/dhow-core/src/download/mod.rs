//! One download, from probe to a fully committed part file: resume or
//! reset, then fill every gap over one or more connections (ENGINE-SPEC §2,
//! §4, §5.4, §6 I3–I5, I7, §7, §9). Digest check and the final rename are
//! the completion gate's (§8).

pub mod ramp;

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use reqwest::{Client, Url};
use tokio::sync::watch;

use crate::http::Header;
use crate::journal::{Header as JournalHeader, Journal, JournalError, format};
use crate::probe::{self, ProbeError, ProbeInfo, ProbeOptions, Resumable};
use crate::rangeset::RangeSet;
use crate::transfer::{
    self, ConnEvent, Cursor, Finished, IdentityChange, OVERLAP, Overlap, Source, TransferConfig,
    TransferError,
};
use crate::writer::actor::{self, Outcome, WriterGone, WriterHandle};
use crate::writer::{self, ChunkId, Writer, WriterConfig, WriterError};
use ramp::{LinkState, Ramp, RampConfig, Step};

pub struct Request {
    pub url: Url,
    pub headers: Vec<Header>,
    pub part: PathBuf,
    pub sidecar: PathBuf,
    /// Overrides `RampConfig::connections` for this download.
    pub connections: Option<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct DownloadConfig {
    pub probe: ProbeOptions,
    pub transfer: TransferConfig,
    pub writer: WriterConfig,
    pub ramp: RampConfig,
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
            ramp: RampConfig::default(),
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
    /// Connections now open.
    Connections(u32),
    /// §9 Protocol: the server stopped honoring `Range` mid-download; the
    /// download continues on one connection.
    Downgraded,
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
    /// The stop signal fired (pause, cancel, shutdown). Everything that
    /// arrived was committed first.
    #[error("stopped")]
    Stopped,
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

/// Resolves once `stop` holds `true`; never if its sender is gone.
async fn stopped(mut stop: watch::Receiver<bool>) {
    if stop.wait_for(|s| *s).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Runs the download until every byte is committed, or until `stop` turns
/// `true` (then [`DownloadError::Stopped`], after a final commit).
pub async fn download(
    req: &Request,
    client: &Client,
    config: &DownloadConfig,
    link: &LinkState,
    on_event: &(dyn Fn(DownloadEvent) + Send + Sync),
    stop: &watch::Receiver<bool>,
) -> Result<Downloaded, DownloadError> {
    let mut resets = 0;
    let mut force = None;
    loop {
        // Re-probe every start: redirects re-resolve, and the identity to
        // resume against is checked against the server's current one.
        let info = tokio::select! {
            biased;
            _ = stopped(stop.clone()) => return Err(DownloadError::Stopped),
            info = probe::probe(client, &req.url, &req.headers, config.probe) => info?,
        };
        on_event(DownloadEvent::Probed(Box::new(info.clone())));
        let (file, journal) = prepare(req, &info, force.take(), on_event)?;
        match fetch(
            req, client, config, link, &info, file, journal, on_event, stop,
        )
        .await
        {
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

#[allow(clippy::too_many_arguments)]
async fn fetch(
    req: &Request,
    client: &Client,
    config: &DownloadConfig,
    link: &LinkState,
    info: &ProbeInfo,
    file: File,
    journal: Journal,
    on_event: &(dyn Fn(DownloadEvent) + Send + Sync),
    stop: &watch::Receiver<bool>,
) -> Result<Downloaded, Stop> {
    let total = info.total_size;
    let committed = journal.committed().clone();
    let gaps = match total {
        Some(t) => committed.gaps(t),
        None => RangeSet::new().gaps(u64::MAX),
    };
    // Overlaps are read before the writer owns the file (I9).
    let mut pending = VecDeque::with_capacity(gaps.len());
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
        pending.push_back((gap, overlap));
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
    let can_range = info.resumable != Resumable::No && total.is_some();
    let rc = &config.ramp;
    let limit = ramp::connection_limit(total, can_range, req.connections, rc, link);
    let (handle, done) = actor::spawn(Writer::new(file, journal, config.writer))?;

    // Shared with every connection's events.
    let received = AtomicU64::new(0);
    let backoff_until = Mutex::new(None::<Instant>);
    let single = AtomicBool::new(limit <= 1);
    let conn_event = |e: ConnEvent| {
        match &e {
            ConnEvent::Received(n) => {
                received.fetch_add(*n, Ordering::Relaxed);
            }
            ConnEvent::Retrying { delay, .. } => {
                *backoff_until.lock().unwrap() = Some(Instant::now() + *delay);
            }
            // §2.3: `Unknown` turned out not to range; stay at one.
            ConnEvent::NotResumable => single.store(true, Ordering::Relaxed),
        }
        on_event(DownloadEvent::Connection(e));
    };

    let mut plan = Plan {
        pending,
        live: HashMap::new(),
        next_id: 0,
    };
    let mut running = FuturesUnordered::new();
    let mut ramp = Ramp::new(Instant::now(), rc);
    ramp.done = limit <= 1;
    let mut connections = 0;
    let mut content_length_seen = true;
    let outcome: Result<(), Option<TransferError>> = 'run: loop {
        let open = running.len() as u32;
        if open != connections {
            connections = open;
            on_event(DownloadEvent::Connections(open));
        }
        if running.is_empty() {
            match plan.next_work(&handle, None, rc).await {
                Ok(Some(work)) => {
                    running.push(connection(
                        &src,
                        work,
                        &handle,
                        &config.transfer,
                        &conn_event,
                    ));
                    continue;
                }
                Ok(None) => break Ok(()),
                Err(WriterGone) => break Err(None),
            }
        }
        let single_now = single.load(Ordering::Relaxed);
        let can_ramp = !ramp.done && !single_now && open < limit;
        if let Some(until) = backoff_until.lock().unwrap().take() {
            ramp.pause_until(until);
        }
        let next_check = ramp.next_check;
        tokio::select! {
            biased;
            _ = stopped(stop.clone()) => break 'run Err(None),
            Some((id, result)) = running.next() => {
                let cursor = plan.live.remove(&id).expect("live chunk");
                match result {
                    Ok(finished) => {
                        content_length_seen = finished.content_length_seen;
                        // Refill the slot: unstarted ranges first, then
                        // steal from the slowest (§4.3). One connection at
                        // a time once downgraded.
                        if !single.load(Ordering::Relaxed) || running.is_empty() {
                            match plan.next_work(&handle, cursor.rate(), rc).await {
                                Ok(Some(work)) => running.push(connection(
                                    &src, work, &handle, &config.transfer, &conn_event,
                                )),
                                Ok(None) => {}
                                Err(WriterGone) => break 'run Err(None),
                            }
                        }
                    }
                    Err(TransferError::IdentityChanged(IdentityChange::RangeIgnored))
                        if !running.is_empty() =>
                    {
                        // §9 Protocol: the server honored ranges and now
                        // ignores them. Give the range back to the chunk it
                        // was split from, whose response still covers it.
                        match plan.merge_back(&handle, id, &cursor).await {
                            Ok(true) => {
                                single.store(true, Ordering::Relaxed);
                                ramp.done = true;
                                tracing::warn!("download: server ignores Range now; one connection");
                                on_event(DownloadEvent::Downgraded);
                            }
                            Ok(false) => {
                                break 'run Err(Some(TransferError::IdentityChanged(
                                    IdentityChange::RangeIgnored,
                                )));
                            }
                            Err(WriterGone) => break 'run Err(None),
                        }
                    }
                    Err(e) => break 'run Err(Some(e)),
                }
            }
            _ = tokio::time::sleep_until(next_check.into()), if can_ramp => {
                let now = Instant::now();
                match ramp.check(now, received.load(Ordering::Relaxed), rc) {
                    Step::Wait => {}
                    Step::Open => {
                        let peers = plan.average_rate();
                        match plan.next_work(&handle, peers, rc).await {
                            Ok(Some(work)) => running.push(connection(
                                &src, work, &handle, &config.transfer, &conn_event,
                            )),
                            // Nothing left worth a new connection.
                            Ok(None) => ramp.done = true,
                            Err(WriterGone) => break 'run Err(None),
                        }
                    }
                    Step::Saturated => {
                        tracing::info!(connections = open, "download: link saturated");
                        link.record_saturation(open, rc.saturation_ttl);
                    }
                }
            }
        }
    };
    // Connections borrow the writer handle; they go first.
    drop(running);
    if connections > 0 {
        on_event(DownloadEvent::Connections(0));
    }
    if let Err(e) = outcome {
        // Let the writer commit what arrived, and stop, before anything
        // touches the files again.
        drop(handle);
        let writer_result = done.await;
        return Err(match (e, writer_result) {
            (_, Ok(Err(we))) => Stop::Failed(we.into()),
            (None, _) => Stop::Failed(DownloadError::Stopped),
            (Some(TransferError::IdentityChanged(change)), _) => {
                tracing::warn!(?change, "download: identity changed");
                Stop::Reset(ResetReason::ServerFileChanged)
            }
            (Some(TransferError::NeedsRestart), _) => Stop::Reset(ResetReason::CannotResume),
            (Some(e), _) => Stop::Failed(e.into()),
        });
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

/// One unit of work for a connection.
struct Work {
    id: ChunkId,
    cursor: Arc<Cursor>,
    overlap: Option<Overlap>,
}

/// What is left to fetch and who is fetching it.
struct Plan {
    /// Ranges nobody has started (resume gaps), in order.
    pending: VecDeque<(std::ops::Range<u64>, Option<Overlap>)>,
    live: HashMap<ChunkId, Arc<Cursor>>,
    next_id: ChunkId,
}

impl Plan {
    /// An unstarted range if any, else a piece stolen from the connection
    /// that would finish last (§4.3). The split is journaled before this
    /// returns, so before the new connection sends anything.
    async fn next_work(
        &mut self,
        writer: &WriterHandle,
        new_rate: Option<f64>,
        config: &RampConfig,
    ) -> Result<Option<Work>, WriterGone> {
        if let Some((range, overlap)) = self.pending.pop_front() {
            let id = self.take_id();
            writer.define(id, range.clone()).await?;
            return Ok(Some(self.start(id, range, overlap)));
        }
        // Slowest to finish first: remaining / rate, unknown rates last.
        let mut owners: Vec<(ChunkId, Arc<Cursor>)> =
            self.live.iter().map(|(id, c)| (*id, c.clone())).collect();
        owners.sort_by(|a, b| eta(&b.1).total_cmp(&eta(&a.1)));
        for (owner, cursor) in owners {
            let Some(at) =
                ramp::split_point(cursor.pos(), cursor.end(), cursor.rate(), new_rate, config)
            else {
                continue;
            };
            let id = self.take_id();
            if let Some(tail) = writer.split(owner, id, at).await? {
                cursor.set_end(at);
                return Ok(Some(self.start(id, tail, None)));
            }
        }
        Ok(None)
    }

    /// Undoes the split that created `id` (§9 Protocol downgrade): the
    /// chunk ending at its start takes it back, if that connection hasn't
    /// got there yet.
    async fn merge_back(
        &mut self,
        writer: &WriterHandle,
        id: ChunkId,
        cursor: &Cursor,
    ) -> Result<bool, WriterGone> {
        let predecessor = self
            .live
            .iter()
            .find(|(_, c)| c.end() == cursor.start() && c.pos() < cursor.start())
            .map(|(pid, c)| (*pid, c.clone()));
        let Some((pid, pc)) = predecessor else {
            return Ok(false);
        };
        if !writer.merge(pid, id).await? {
            return Ok(false);
        }
        pc.set_end(cursor.end());
        Ok(true)
    }

    fn take_id(&mut self) -> ChunkId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn start(
        &mut self,
        id: ChunkId,
        range: std::ops::Range<u64>,
        overlap: Option<Overlap>,
    ) -> Work {
        let cursor = Arc::new(Cursor::new(range));
        self.live.insert(id, cursor.clone());
        Work {
            id,
            cursor,
            overlap,
        }
    }

    /// Typical speed of the open connections: the guess for a new one.
    fn average_rate(&self) -> Option<f64> {
        let rates: Vec<f64> = self.live.values().filter_map(|c| c.rate()).collect();
        (!rates.is_empty()).then(|| rates.iter().sum::<f64>() / rates.len() as f64)
    }
}

fn eta(c: &Cursor) -> f64 {
    match c.rate() {
        Some(r) => c.remaining() as f64 / r,
        None => 0.0,
    }
}

async fn connection(
    src: &Source,
    work: Work,
    writer: &WriterHandle,
    config: &TransferConfig,
    on_event: &(dyn Fn(ConnEvent) + Send + Sync),
) -> (ChunkId, Result<Finished, TransferError>) {
    let result = transfer::run_connection(
        src,
        work.id,
        &work.cursor,
        work.overlap,
        writer,
        config,
        on_event,
    )
    .await;
    (work.id, result)
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
