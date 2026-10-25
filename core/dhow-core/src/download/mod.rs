//! One download, from probe to a fully committed part file: resume or
//! reset, then fill every gap over one or more connections (ENGINE-SPEC §2,
//! §4, §5.4, §6 I3–I5, I7, §7, §9). Digest check and the final rename are
//! the completion gate's (§8).

pub mod budget;
pub mod hold;
pub mod ramp;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use reqwest::{Client, Url};
use tokio::sync::{Notify, watch};

use crate::http::Header;
use crate::journal::{Header as JournalHeader, Journal, JournalError, format};
use crate::limit::{Bucket, GlobalLimit, Limiter};
use crate::probe::{self, ProbeError, ProbeInfo, ProbeOptions, Resumable};
use crate::rangeset::RangeSet;
use crate::transfer::{
    self, ConnEvent, Cursor, Finished, IdentityChange, OVERLAP, Overlap, Source, TransferConfig,
    TransferError,
};
use crate::writer::actor::{self, Outcome, WriterGone, WriterHandle};
use crate::writer::{self, ChunkId, Writer, WriterConfig, WriterError};
use budget::{Budget, BudgetConfig, Ticket};
use hold::PartHold;
use ramp::{LinkState, Ramp, RampConfig, Sample, Step};

/// Range maps at most this often (ARCHITECTURE §3.3).
const MAP_EVERY: Duration = Duration::from_secs(1);

pub struct Request {
    pub url: Url,
    pub headers: Vec<Header>,
    pub part: PathBuf,
    pub sidecar: PathBuf,
    /// Overrides `RampConfig::connections` for this download.
    pub connections: Option<u8>,
    /// When it first started (unix ms): older downloads get connections
    /// first (§10.3). `None`: now.
    pub started_at: Option<i64>,
    /// Speed limits besides the global one: its queue's and its own (§12).
    pub limits: Vec<Arc<Bucket>>,
    /// The caller's hold on `part`, when its run touches the files outside
    /// [`download`] too (recovery, finalize). `None`: `download` takes one,
    /// so a caller already holding `part` that passes `None` deadlocks.
    pub hold: Option<PartHold>,
    /// Added in blind mode (§2.4): a probe that fails doesn't fail it, it
    /// goes on blind.
    pub blind: bool,
    /// What the caller may change while it runs; its `connections` wins
    /// over the field above.
    pub tuning: Option<Tuning>,
}

/// What a caller changes on a running download (ARCHITECTURE §3.1).
#[derive(Clone, Debug)]
pub struct Tuning {
    /// Its connection override (§4.1). A higher `n` resumes ramp-up
    /// (§4.4); a lower one stops the newest connections at once, their rest
    /// going back to pending work as for a newcomer (§10.3).
    pub connections: watch::Receiver<Option<u8>>,
    /// While `true`: a [`DownloadEvent::Ranges`] at once, then at most one
    /// a second.
    pub ranges: watch::Receiver<bool>,
}

/// Where a download's bytes are: the segment map (ARCHITECTURE §3.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeMap {
    /// `None`: blind (§2.4); the last range then ends at `u64::MAX`.
    pub total: Option<u64>,
    /// Fetched. Not running: committed (the sidecar). Running: written, and
    /// committed within the commit interval.
    pub done: Vec<Range<u64>>,
    /// One per open connection, by `start`.
    pub connections: Vec<ConnectionRange>,
    /// Nobody fetches these yet.
    pub pending: Vec<Range<u64>>,
}

/// A connection's chunk: `[start, at)` received, `[at, end)` to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionRange {
    pub start: u64,
    pub at: u64,
    pub end: u64,
}

/// What all downloads in a process share: the connection budget (§10.1–
/// 10.3), what is known about the link (§10.A) and the global speed limit
/// (§12).
#[derive(Debug)]
pub struct Shared {
    pub link: LinkState,
    pub budget: Arc<Budget>,
    pub global: Arc<GlobalLimit>,
    /// Woken by every limit change; buckets made for this process's
    /// downloads must share it ([`Shared::bucket`]).
    limits_changed: Arc<Notify>,
}

impl Default for Shared {
    fn default() -> Self {
        Self::new(BudgetConfig::default())
    }
}

impl Shared {
    pub fn new(budget: BudgetConfig) -> Self {
        let limits_changed = Arc::new(Notify::new());
        Self {
            link: LinkState::default(),
            budget: Arc::new(Budget::new(budget)),
            global: Arc::new(GlobalLimit::new(limits_changed.clone())),
            limits_changed,
        }
    }

    /// A queue's or a download's speed limit.
    pub fn bucket(&self, rate: Option<u64>) -> Arc<Bucket> {
        Arc::new(Bucket::new(rate, self.limits_changed.clone()))
    }
}

/// Tuning fixed for an engine's life. In an engine, `probe.range_get`,
/// `ramp.connections` and `budget` are only the defaults of their settings
/// (`engine::settings`).
#[derive(Clone, Copy, Debug)]
pub struct DownloadConfig {
    pub probe: ProbeOptions,
    pub transfer: TransferConfig,
    pub writer: WriterConfig,
    pub ramp: RampConfig,
    /// For the process-wide budget built from this config (see [`Shared`]).
    pub budget: BudgetConfig,
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
            budget: BudgetConfig::default(),
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
    /// `true`: waiting for the connection budget to allow a first
    /// connection (`Waiting(ConnectionBudget)`); `false`: got it.
    Waiting(bool),
    /// §9 Protocol: the server stopped honoring `Range` mid-download; the
    /// download continues on one connection.
    Downgraded,
    Connection(ConnEvent),
    /// While [`Tuning::ranges`] is on.
    Ranges(RangeMap),
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
pub(crate) async fn stopped(mut stop: watch::Receiver<bool>) {
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
    shared: &Shared,
    on_event: &(dyn Fn(DownloadEvent) + Send + Sync),
    stop: &watch::Receiver<bool>,
) -> Result<Downloaded, DownloadError> {
    let hold = match &req.hold {
        Some(h) => h.clone(),
        None => tokio::select! {
            biased;
            _ = stopped(stop.clone()) => return Err(DownloadError::Stopped),
            h = PartHold::acquire(&req.part) => h,
        },
    };
    let mut resets = 0;
    let mut force = None;
    let mut ticket = None;
    loop {
        // Re-probe every start: redirects re-resolve, and the identity to
        // resume against is checked against the server's current one.
        let probed = tokio::select! {
            biased;
            _ = stopped(stop.clone()) => return Err(DownloadError::Stopped),
            info = probe::probe(client, &req.url, &req.headers, config.probe) => info,
        };
        let info = match probed {
            Ok(info) => info,
            Err(e) if req.blind && e.allows_blind() => {
                tracing::info!(error = %e, "download: probe failed, going on blind");
                ProbeInfo::blind(&req.url)
            }
            Err(e) => return Err(e.into()),
        };
        on_event(DownloadEvent::Probed(Box::new(info.clone())));
        let ticket = ticket.get_or_insert_with(|| {
            let started_at = req.started_at.unwrap_or_else(now_ms);
            shared
                .budget
                .register(started_at, host_key(&info.effective_url))
        });
        // Claimed from here, not from the first connection: prepare can
        // take a while, and younger downloads must not ramp into this
        // one's share meanwhile (§10.3).
        let can_range = info.resumable != Resumable::No && info.total_size.is_some();
        let requested = req
            .tuning
            .as_ref()
            .map_or(req.connections, |t| *t.connections.borrow());
        let limit = ramp::connection_limit(
            info.total_size,
            can_range,
            requested,
            &config.ramp,
            &shared.link,
        );
        ticket.set_limit(limit);
        let (file, journal) = prepare(req, &hold, &info, force.take(), on_event).await?;
        let fetched = fetch(
            req, client, config, shared, ticket, limit, &info, file, journal, &hold, on_event, stop,
        )
        .await;
        ticket.release_all();
        match fetched {
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

/// What [`prepare`] works on, owned for its blocking thread.
struct Files {
    url: Url,
    part: PathBuf,
    sidecar: PathBuf,
}

/// Opens the journal and part file to resume, or discards both and starts
/// fresh, emitting `Resumed` or `Reset` accordingly. The file work (opens,
/// replay, preallocation, fsyncs) runs off the async workers, in the same
/// order. If this future is dropped, that work still finishes, holding
/// `hold` until it has: it leaves what a crash at the same point would
/// (§5.4), and a rerun on the same files waits for it.
async fn prepare(
    req: &Request,
    hold: &PartHold,
    info: &ProbeInfo,
    force: Option<ResetReason>,
    on_event: &(dyn Fn(DownloadEvent) + Send + Sync),
) -> Result<(File, Journal), DownloadError> {
    let files = Files {
        url: req.url.clone(),
        part: req.part.clone(),
        sidecar: req.sidecar.clone(),
    };
    let (info, hold) = (info.clone(), hold.clone());
    // The hold last: a dropped run's result is dropped in order, so its
    // files are closed before a rerun gets them (Windows delete-pending).
    let (prepared, _hold) =
        tokio::task::spawn_blocking(move || (prepare_files(&files, &info, force), hold))
            .await
            .map_err(io::Error::other)?;
    let (file, journal, prepared) = prepared?;
    match prepared {
        Prepared::Resumed => on_event(DownloadEvent::Resumed {
            committed: journal.committed().len(),
        }),
        Prepared::Fresh(None) => {}
        Prepared::Fresh(Some(reason)) => {
            tracing::info!(?reason, "download: starting over");
            on_event(DownloadEvent::Reset(reason));
        }
    }
    Ok((file, journal))
}

enum Prepared {
    Resumed,
    /// Why what was on disk was discarded; `None`: nothing was.
    Fresh(Option<ResetReason>),
}

/// [`prepare`]'s blocking part.
fn prepare_files(
    f: &Files,
    info: &ProbeInfo,
    force: Option<ResetReason>,
) -> Result<(File, Journal, Prepared), DownloadError> {
    let reason = match force {
        Some(reason) => Some(reason),
        None => match resumable_state(f, info)? {
            Ok((file, journal)) => return Ok((file, journal, Prepared::Resumed)),
            Err(reason) => reason,
        },
    };
    #[cfg(test)]
    hold::park::at("prepare", &f.part);
    // Sidecar first: a crash in between leaves a part without a sidecar,
    // which is discarded on the next start (§5.4), never trusted.
    remove_if_exists(&f.sidecar)?;
    remove_if_exists(&f.part)?;
    let (file, preallocated) = writer::create_part(&f.part, info.total_size)?;
    let header = JournalHeader {
        preallocated,
        total_size: info.total_size,
        url_hash: format::url_hash(f.url.as_str()),
        etag: info.etag.clone(),
        last_modified: info.last_modified.as_deref().and_then(unix_secs),
        created_at: now_ms(),
    };
    let journal = Journal::create(&f.sidecar, header)?;
    Ok((file, journal, Prepared::Fresh(reason)))
}

/// `Ok(Ok(..))`: resume from these. `Ok(Err(None))`: nothing on disk.
/// `Ok(Err(Some(reason)))`: what's on disk can't be used.
#[allow(clippy::type_complexity)]
fn resumable_state(
    f: &Files,
    info: &ProbeInfo,
) -> Result<Result<(File, Journal), Option<ResetReason>>, DownloadError> {
    let part_exists = f.part.try_exists()?;
    let journal = match Journal::open(&f.sidecar) {
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
    if !same_identity(journal.header(), &f.url, info) {
        return Ok(Err(Some(ResetReason::ServerFileChanged)));
    }
    let can_range = info.resumable != Resumable::No && info.total_size.is_some();
    if !journal.committed().is_empty() && !can_range {
        return Ok(Err(Some(ResetReason::CannotResume)));
    }
    let file = OpenOptions::new().read(true).write(true).open(&f.part)?;
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
fn same_identity(h: &JournalHeader, url: &Url, info: &ProbeInfo) -> bool {
    h.url_hash == format::url_hash(url.as_str())
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
    shared: &Shared,
    ticket: &Ticket,
    mut limit: u32,
    info: &ProbeInfo,
    file: File,
    journal: Journal,
    hold: &PartHold,
    on_event: &(dyn Fn(DownloadEvent) + Send + Sync),
    stop: &watch::Receiver<bool>,
) -> Result<Downloaded, Stop> {
    let total = info.total_size;
    let can_range = info.resumable != Resumable::No && total.is_some();
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
        pending.push_back(Pending {
            range: gap,
            overlap,
            defined: None,
        });
    }

    let validator = transfer::validator(info.etag.as_deref(), info.last_modified.as_deref());
    let limiter = Arc::new(Limiter::new(shared.global.clone(), req.limits.clone()));
    let src = Source {
        client: client.clone(),
        url: req.url.clone(),
        headers: req.headers.clone(),
        total,
        resumable: info.resumable,
        validator: validator.clone(),
        limiter: limiter.clone(),
    };
    let link = &shared.link;
    let rc = &config.ramp;
    // A dropped run's writer still makes its final commit; until then the
    // files are its.
    let writer = Writer::new(file, journal, config.writer);
    let (handle, done) = actor::spawn_holding(writer, hold.clone())?;

    // Shared with every connection's events.
    let received = AtomicU64::new(0);
    let backoff_until = Mutex::new(None::<Instant>);
    // One connection for good: the server can do no more. (A limit of 1
    // can be raised, `Tuning`.)
    let single = AtomicBool::new(!can_range);
    let conn_event = |e: ConnEvent| {
        match &e {
            ConnEvent::Received(n) => {
                received.fetch_add(*n, Ordering::Relaxed);
            }
            ConnEvent::Retrying { delay, .. } => {
                *backoff_until.lock().unwrap() = Some(Instant::now() + *delay);
            }
            // §2.3: `Unknown` turned out not to range; stay at one.
            ConnEvent::NotResumable => {
                single.store(true, Ordering::Relaxed);
                ticket.stop_wanting();
            }
        }
        on_event(DownloadEvent::Connection(e));
    };

    let mut plan = Plan {
        pending,
        live: HashMap::new(),
        shedding: HashSet::new(),
        next_id: 0,
    };
    let mut running = FuturesUnordered::new();
    let mut ramp = Ramp::new(Instant::now(), rc);
    ramp.done = limit <= 1;
    // Headroom's cap is lifted while this download ramps up (§10.6).
    let mut learner = shared.global.learner();
    let mut connections = 0;
    let mut content_length_seen = true;
    let mut tuning = req.tuning.clone();
    let mut watched = tuning
        .as_mut()
        .is_some_and(|t| *t.ranges.borrow_and_update());
    let mut next_map = Instant::now();
    let outcome: Result<(), Option<TransferError>> = 'run: loop {
        let open = running.len() as u32;
        if open != connections {
            connections = open;
            on_event(DownloadEvent::Connections(open));
        }
        // Registered first, so a budget change after the checks below
        // still wakes this loop.
        let changed = ticket.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();

        if running.is_empty() {
            if plan.pending.is_empty() {
                break Ok(());
            }
            // Every connection holds a budget slot; the first may wait.
            if ticket.held() == 0 && !ticket.try_first() {
                on_event(DownloadEvent::Waiting(true));
                tokio::select! {
                    biased;
                    _ = stopped(stop.clone()) => break 'run Err(None),
                    _ = ticket.first() => {}
                }
                on_event(DownloadEvent::Waiting(false));
            }
            match plan.next_work(&handle, None, rc).await {
                Ok(Some(work)) => running.push(connection(
                    &src,
                    work,
                    &handle,
                    &config.transfer,
                    &conn_event,
                )),
                Ok(None) => unreachable!("pending work exists"),
                Err(WriterGone) => break Err(None),
            }
            continue;
        }
        // §10.3: the youngest download with spare connections gives one up
        // for a newcomer.
        while ticket.shed_requested() as usize > plan.shedding.len()
            && running.len() > plan.shedding.len() + 1
        {
            match plan.shed(&handle).await {
                Ok(true) => {}
                Ok(false) => break,
                Err(WriterGone) => break 'run Err(None),
            }
        }
        // Above a lowered limit (`Tuning`): the newest stop where they are,
        // their rest is pending. One still verifying its overlap goes on a
        // later pass, once it has.
        while running.len() > plan.shedding.len() + limit as usize {
            match plan.shed(&handle).await {
                Ok(true) => {}
                Ok(false) => break,
                Err(WriterGone) => break 'run Err(None),
            }
        }
        let single_now = single.load(Ordering::Relaxed);
        let can_ramp = !ramp.done && !single_now && open < limit;
        learner.set(can_ramp && ramp.probing());
        if let Some(until) = backoff_until.lock().unwrap().take() {
            ramp.pause_until(until);
        }
        let next_check = ramp.next_check;
        tokio::select! {
            biased;
            _ = stopped(stop.clone()) => break 'run Err(None),
            Some((id, result)) = running.next() => {
                let cursor = plan.live.remove(&id).expect("live chunk").cursor;
                if plan.shedding.remove(&id) {
                    // Given up for a newcomer: its slot goes there.
                    ticket.release();
                    continue;
                }
                match result {
                    Ok(finished) => {
                        content_length_seen = finished.content_length_seen;
                        // Refill the slot: pending ranges first, then steal
                        // from the slowest (§4.3). One connection at a time
                        // once downgraded; none above a lowered limit.
                        let refill = (!single.load(Ordering::Relaxed) || running.is_empty())
                            && running.len() - plan.shedding.len() < limit as usize;
                        let work = if refill {
                            match plan.next_work(&handle, cursor.rate(), rc).await {
                                Ok(w) => w,
                                Err(WriterGone) => break 'run Err(None),
                            }
                        } else {
                            None
                        };
                        match work {
                            Some(work) => running.push(connection(
                                &src, work, &handle, &config.transfer, &conn_event,
                            )),
                            None => ticket.release(),
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
                                ticket.release();
                                single.store(true, Ordering::Relaxed);
                                ramp.done = true;
                                ticket.stop_wanting();
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
                let sample = Sample {
                    bytes: received.load(Ordering::Relaxed),
                    waits: limiter.waits(),
                    aggregate: shared.global.total(),
                };
                let step = ramp.check(now, sample, rc);
                if let Some(rate) = ramp.take_aggregate() {
                    shared.global.offer_peak(rate);
                }
                match step {
                    Step::Wait => {}
                    // The budget decides whether ramp-up may have it.
                    Step::Open if !ticket.try_grow() => ramp.blocked(now, rc),
                    Step::Open => {
                        let peers = plan.average_rate();
                        match plan.next_work(&handle, peers, rc).await {
                            Ok(Some(work)) => running.push(connection(
                                &src, work, &handle, &config.transfer, &conn_event,
                            )),
                            // Nothing left worth a new connection.
                            Ok(None) => {
                                ticket.release();
                                ramp.done = true;
                                ticket.stop_wanting();
                            }
                            Err(WriterGone) => break 'run Err(None),
                        }
                    }
                    Step::Saturated => {
                        tracing::info!(connections = open, "download: link saturated");
                        link.record_saturation(open, rc.saturation_ttl);
                        ticket.stop_wanting();
                    }
                }
            }
            _ = tokio::time::sleep_until(next_map.into()), if watched => {
                on_event(DownloadEvent::Ranges(plan.map(total)));
                next_map = Instant::now() + MAP_EVERY;
            }
            tuned = tuned(&mut tuning) => match tuned {
                Tuned::Ranges(on) => {
                    watched = on;
                    next_map = Instant::now();
                }
                Tuned::Connections(n) => {
                    let before = limit;
                    // Downgraded (§2.3, §9 Protocol): one, whatever is asked.
                    limit = match single_now {
                        true => 1,
                        false => ramp::connection_limit(total, can_range, n, rc, link),
                    };
                    ticket.set_limit(limit);
                    // Ramp-up resumes only for a higher limit; saturation or
                    // nothing left to split stand otherwise.
                    if ramp.done && limit > before && open < limit {
                        ramp = Ramp::new(Instant::now(), rc);
                    }
                    if ramp.done || open >= limit {
                        // Not ramping: no claim ahead of younger downloads
                        // (§10.3).
                        ticket.stop_wanting();
                    }
                    // Fewer: the pass sheds the newest.
                }
            },
            // A newcomer may need one of ours.
            _ = &mut changed => {}
        }
    };
    // Connections borrow the writer handle; they go first.
    drop(running);
    drop(learner);
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

enum Tuned {
    Connections(Option<u8>),
    Ranges(bool),
}

/// The caller's next change, or never (no tuning, or the caller gone).
async fn tuned(tuning: &mut Option<Tuning>) -> Tuned {
    let Some(t) = tuning else {
        return std::future::pending().await;
    };
    // Connections first: a later change of both is seen in that order.
    let changed = tokio::select! {
        biased;
        r = t.connections.changed() => {
            r.map(|()| Tuned::Connections(*t.connections.borrow_and_update()))
        }
        r = t.ranges.changed() => r.map(|()| Tuned::Ranges(*t.ranges.borrow_and_update())),
    };
    match changed {
        Ok(t) => t,
        Err(_) => {
            *tuning = None;
            std::future::pending().await
        }
    }
}

/// `host:port`, the per-host budget key (§10.2).
fn host_key(url: &Url) -> String {
    format!(
        "{}:{}",
        url.host_str().unwrap_or_default(),
        url.port_or_known_default().unwrap_or(0)
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// One unit of work for a connection.
struct Work {
    id: ChunkId,
    cursor: Arc<Cursor>,
    overlap: Option<Overlap>,
    cancel: Arc<Notify>,
}

/// A range nobody is fetching.
struct Pending {
    range: std::ops::Range<u64>,
    overlap: Option<Overlap>,
    /// Already a chunk in the writer (given back by a shed connection).
    defined: Option<ChunkId>,
}

struct Live {
    cursor: Arc<Cursor>,
    cancel: Arc<Notify>,
    /// Started with an overlap to verify (I5); not shed before it has.
    verifies: bool,
}

/// What is left to fetch and who is fetching it.
struct Plan {
    /// Ranges nobody is fetching, in order.
    pending: VecDeque<Pending>,
    live: HashMap<ChunkId, Live>,
    /// Connections being stopped for a newcomer (§10.3).
    shedding: HashSet<ChunkId>,
    next_id: ChunkId,
}

impl Plan {
    /// A pending range if any, else a piece stolen from the connection
    /// that would finish last (§4.3). The split is journaled before this
    /// returns, so before the new connection sends anything.
    async fn next_work(
        &mut self,
        writer: &WriterHandle,
        new_rate: Option<f64>,
        config: &RampConfig,
    ) -> Result<Option<Work>, WriterGone> {
        if let Some(p) = self.pending.pop_front() {
            let id = match p.defined {
                Some(id) => id,
                None => {
                    let id = self.take_id();
                    writer.define(id, p.range.clone()).await?;
                    id
                }
            };
            return Ok(Some(self.start(id, p.range, p.overlap)));
        }
        // Slowest to finish first: remaining / rate, unknown rates last.
        let mut owners: Vec<(ChunkId, Arc<Cursor>)> = self
            .live
            .iter()
            .filter(|(id, _)| !self.shedding.contains(id))
            .map(|(id, l)| (*id, l.cursor.clone()))
            .collect();
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

    /// Stops the newest connection where it is; the rest of its chunk goes
    /// back to the front of the pending queue. Returns whether one was found.
    async fn shed(&mut self, writer: &WriterHandle) -> Result<bool, WriterGone> {
        let candidate = self
            .live
            .iter()
            .filter(|(id, l)| {
                !self.shedding.contains(id) && !(l.verifies && l.cursor.pos() == l.cursor.start())
            })
            .max_by_key(|(id, _)| **id)
            .map(|(id, l)| (*id, l.cursor.clone(), l.cancel.clone()));
        let Some((id, cursor, cancel)) = candidate else {
            return Ok(false);
        };
        let at = cursor.pos();
        let tail_id = self.take_id();
        if let Some(tail) = writer.split(id, tail_id, at).await? {
            cursor.set_end(at);
            self.pending.push_front(Pending {
                range: tail,
                overlap: None,
                defined: Some(tail_id),
            });
        }
        self.shedding.insert(id);
        cancel.notify_one();
        Ok(true)
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
            .find(|(_, l)| l.cursor.end() == cursor.start() && l.cursor.pos() < cursor.start())
            .map(|(pid, l)| (*pid, l.cursor.clone()));
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
        let cancel = Arc::new(Notify::new());
        self.live.insert(
            id,
            Live {
                cursor: cursor.clone(),
                cancel: cancel.clone(),
                verifies: overlap.is_some(),
            },
        );
        Work {
            id,
            cursor,
            overlap,
            cancel,
        }
    }

    /// The segment map now: what no connection or pending work covers is
    /// done.
    fn map(&self, total: Option<u64>) -> RangeMap {
        let end = total.unwrap_or(u64::MAX);
        let mut connections: Vec<ConnectionRange> = self
            .live
            .values()
            .map(|l| ConnectionRange {
                start: l.cursor.start(),
                at: l.cursor.pos(),
                end: l.cursor.end().min(end),
            })
            .collect();
        connections.sort_by_key(|c| c.start);
        let pending: Vec<Range<u64>> = self
            .pending
            .iter()
            .map(|p| p.range.start..p.range.end.min(end))
            .collect();
        let mut busy = RangeSet::new();
        for r in connections.iter().map(|c| c.start..c.end) {
            busy.insert(r);
        }
        for r in &pending {
            busy.insert(r.clone());
        }
        RangeMap {
            total,
            done: busy.gaps(end),
            connections,
            pending,
        }
    }

    /// Typical speed of the open connections: the guess for a new one.
    fn average_rate(&self) -> Option<f64> {
        let rates: Vec<f64> = self.live.values().filter_map(|l| l.cursor.rate()).collect();
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
    let Work {
        id,
        cursor,
        overlap,
        cancel,
    } = work;
    tokio::select! {
        biased;
        // Shed (§10.3): stop here; the rest of the chunk is pending again.
        _ = cancel.notified() => (id, Ok(Finished { content_length_seen: true })),
        r = transfer::run_connection(src, id, &cursor, overlap, writer, config, on_event) => (id, r),
    }
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dhow_testserver::TestServer;

    use super::hold::park;
    use super::*;

    /// An elder parked in prepare while a younger download, holding its
    /// first connection, asks for more on a budget of 4.
    struct Fx {
        shared: Arc<Shared>,
        armed: park::Armed,
        elder: tokio::task::JoinHandle<Result<Downloaded, DownloadError>>,
        /// The elder's connections, as its events report them.
        open: watch::Receiver<u32>,
        young: Ticket,
        _server: TestServer,
        _dir: tempfile::TempDir,
    }

    async fn elder_in_prepare() -> Fx {
        let shared = Arc::new(Shared::new(BudgetConfig {
            global: 4,
            per_host: 4,
            min_for_new: 1,
        }));
        let server = TestServer::start().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let req = Request {
            url: Url::parse(&server.url("/blob/3000000?throttle=500000")).unwrap(),
            headers: vec![],
            part: dir.path().join("a.part"),
            sidecar: dir.path().join("a.dhow"),
            connections: Some(4),
            started_at: Some(1),
            limits: vec![],
            hold: None,
            blind: false,
            tuning: None,
        };
        let armed = park::arm("prepare", &req.part);
        let p = armed.park();
        let reached = tokio::task::spawn_blocking(move || p.wait_reached());
        let (tx, open) = watch::channel(0);
        let s = shared.clone();
        let elder = tokio::spawn(async move {
            let config = DownloadConfig {
                ramp: RampConfig {
                    window: Duration::from_millis(60),
                    min_chunk: 128 * 1024,
                    // Never saturated: this is about the budget.
                    min_gain: -1.0,
                    ..RampConfig::default()
                },
                ..DownloadConfig::default()
            };
            let on_event = move |e: DownloadEvent| {
                if let DownloadEvent::Connections(n) = e {
                    tx.send_replace(n);
                }
            };
            let (_keep, stop) = watch::channel(false);
            let client = crate::http::client().unwrap();
            download(&req, &client, &config, &s, &on_event, &stop).await
        });
        tokio::time::timeout(Duration::from_secs(10), reached)
            .await
            .expect("the elder parked in prepare")
            .unwrap();
        let young = shared.budget.register(2, "young:80".into());
        young.set_limit(4);
        assert!(young.try_first(), "a first connection needs only the caps");
        Fx {
            shared,
            armed,
            elder,
            open,
            young,
            _server: server,
            _dir: dir,
        }
    }

    /// §10.3: a download claims its share from the moment it is
    /// registered, not from its first connection, so a younger one can't
    /// ramp into it while the elder's prepare is still on disk.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_elder_in_prepare_keeps_its_claim() {
        let mut fx = elder_in_prepare().await;
        assert!(
            !fx.young.try_grow(),
            "grew into the preparing elder's claim"
        );
        fx.armed.release();
        tokio::time::timeout(Duration::from_secs(30), fx.open.wait_for(|n| *n == 3))
            .await
            .expect("the elder ramped into the rest of the budget")
            .unwrap();
        assert!(!fx.young.try_grow());
        assert_eq!(fx.shared.budget.usage("").0, 4);
        drop(fx.young);
        fx.elder.await.unwrap().unwrap();
    }

    /// A run dropped during prepare gives its claim back with its ticket.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_dropped_in_prepare_releases_its_claim() {
        let fx = elder_in_prepare().await;
        assert!(!fx.young.try_grow());
        fx.elder.abort();
        assert!(fx.elder.await.unwrap_err().is_cancelled());
        assert!(fx.young.try_grow() && fx.young.try_grow() && fx.young.try_grow());
        drop(fx.armed);
    }

    /// Asked for more connections once downgraded to one (§2.3: `Unknown`
    /// turned out not to range), a download stays at one and claims
    /// nothing ahead of younger downloads (§10.3).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_downgraded_download_asked_for_more_claims_nothing() {
        downgraded_then_asked_for_more(false).await;
    }

    /// The same when the request lands in the very pass of the fetch loop
    /// that saw the downgrade: it used to go by `single` as read before
    /// that pass, set the limit back to 4 and claim 3 for good.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_downgrade_and_a_request_in_one_pass_claim_nothing() {
        downgraded_then_asked_for_more(true).await;
    }

    /// `in_pass`: asked from the downgrade's own event, so before the
    /// fetch loop polls its next branch.
    async fn downgraded_then_asked_for_more(in_pass: bool) {
        let shared = Arc::new(Shared::new(BudgetConfig {
            global: 4,
            per_host: 4,
            min_for_new: 1,
        }));
        let server = TestServer::start().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (connections, connections_rx) = watch::channel(Some(4));
        let connections = Arc::new(connections);
        let (ranges, ranges_rx) = watch::channel(false);
        let req = Request {
            // No `Accept-Ranges`, and `Range` ignored.
            url: Url::parse(&server.url("/blob/3000000?ranges=ignore&throttle=300000")).unwrap(),
            headers: vec![],
            part: dir.path().join("d.part"),
            sidecar: dir.path().join("d.dhow"),
            connections: Some(4),
            started_at: Some(1),
            limits: vec![],
            hold: None,
            blind: false,
            tuning: Some(Tuning {
                connections: connections_rx,
                ranges: ranges_rx,
            }),
        };
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let (stop_tx, stop) = watch::channel(false);
        let s = shared.clone();
        let ask = connections.clone();
        let elder = tokio::spawn(async move {
            let mut config = DownloadConfig::default();
            config.probe.range_get = false;
            config.ramp.min_chunk = 128 * 1024;
            let on_event = move |e: DownloadEvent| {
                if in_pass && matches!(e, DownloadEvent::Connection(ConnEvent::NotResumable)) {
                    ask.send_replace(Some(4));
                }
                let _ = tx.send(e);
            };
            let client = crate::http::client().unwrap();
            download(&req, &client, &config, &s, &on_event, &stop).await
        });
        let mut until = async |what: &str, pred: &dyn Fn(&DownloadEvent) -> bool| {
            let wait = async {
                while let Some(e) = events.recv().await {
                    if pred(&e) {
                        return;
                    }
                }
                panic!("events ended before {what}");
            };
            tokio::time::timeout(Duration::from_secs(30), wait)
                .await
                .unwrap_or_else(|_| panic!("no {what}"));
        };
        until("downgrade", &|e| {
            matches!(e, DownloadEvent::Connection(ConnEvent::NotResumable))
        })
        .await;
        let young = shared.budget.register(2, "young:80".into());
        young.set_limit(4);
        assert!(young.try_first());

        if !in_pass {
            connections.send_replace(Some(4));
        }
        // Seen after the connections (`tuned` is biased): a fence.
        ranges.send_replace(true);
        until("a map", &|e| matches!(e, DownloadEvent::Ranges(_))).await;
        assert!(young.try_grow(), "the downgraded elder still claims");
        // The elder's one connection and the young's two.
        assert_eq!(shared.budget.usage("").0, 3);

        stop_tx.send_replace(true);
        assert!(matches!(elder.await.unwrap(), Err(DownloadError::Stopped)));
    }
}
