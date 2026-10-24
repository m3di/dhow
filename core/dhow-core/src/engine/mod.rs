//! Downloads as one process manages them: rows in the database, run
//! through probe → [`download`] → completion gate. Every state change is
//! persisted before it is announced.
//!
//! This is the M1 slice of the `Dhow` object (ARCHITECTURE §3): no queues,
//! scheduler or concurrency yet. Running a download is the caller's
//! decision (the CLI runs one in the foreground); the caller also makes
//! sure no two processes run the same download.

pub mod categories;
pub mod groups;
pub mod limits;
mod proxy;
pub mod queues;
pub mod speed;
#[cfg(test)]
mod tests;
mod tokens;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::Url;
use rusqlite::{OptionalExtension, Row, params};
use tokio::sync::watch;

use crate::complete::{self, CompleteError, Target};
use crate::download::{
    self, DownloadConfig, DownloadError, DownloadEvent, Request, ResetReason, Shared,
};
use crate::http::{self, Header};
use crate::journal::format;
use crate::naming::{self, Category, TargetFs};
use crate::platform::{AdapterError, NetworkInfo, NoopAdapter, PlatformAdapter, ProxyConfig};
use crate::probe::{self, ProbeError, Resumable};
use crate::speed::{Ema, rollup_cutoff};
use crate::storage::{DEFAULT_QUEUE_ID, Db, StorageError};
use crate::transfer::{ConnEvent, TransferError};
use crate::writer::WriterError;

/// Progress events at most this often per download (ARCHITECTURE §3.3).
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
const SAMPLE_EVERY: Duration = Duration::from_secs(1);

pub type DownloadId = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Queued,
    /// Waiting for the connection budget (§10.3).
    Waiting,
    Active,
    Paused,
    Completing,
    Completed,
    Failed,
    Cancelled,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Waiting => "waiting",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completing => "completing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "waiting" => Self::Waiting,
            "active" => Self::Active,
            "paused" => Self::Paused,
            "completing" => Self::Completing,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// Nothing more will happen to it.
    pub fn is_final(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailReason {
    /// Network trouble outlasted the retries (§9); `Retry` continues.
    Transient,
    /// The server refused (4xx other than the identity cases).
    Http(u16),
    TooManyRedirects,
    /// Bad URL scheme or header.
    InvalidRequest,
    NoSpace,
    DigestMismatch,
    ServerSentExtraData,
    /// Restarted `max_resets` times and the file kept changing.
    ServerKeptChanging,
    Io,
}

impl FailReason {
    pub fn as_db(&self) -> String {
        match self {
            Self::Transient => "transient".into(),
            Self::Http(code) => format!("http:{code}"),
            Self::TooManyRedirects => "too_many_redirects".into(),
            Self::InvalidRequest => "invalid_request".into(),
            Self::NoSpace => "no_space".into(),
            Self::DigestMismatch => "digest_mismatch".into(),
            Self::ServerSentExtraData => "server_sent_extra_data".into(),
            Self::ServerKeptChanging => "server_kept_changing".into(),
            Self::Io => "io".into(),
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "transient" => Self::Transient,
            "too_many_redirects" => Self::TooManyRedirects,
            "invalid_request" => Self::InvalidRequest,
            "no_space" => Self::NoSpace,
            "digest_mismatch" => Self::DigestMismatch,
            "server_sent_extra_data" => Self::ServerSentExtraData,
            "server_kept_changing" => Self::ServerKeptChanging,
            other => other
                .strip_prefix("http:")
                .and_then(|c| c.parse().ok())
                .map_or(Self::Io, Self::Http),
        }
    }
}

impl fmt::Display for FailReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_db())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarningKind {
    /// The server can't pause/resume or use several connections (§2.3).
    NotResumable,
    /// Blind mode: the size could not be checked (I7).
    SizeUnverified,
    /// No validator: only overlap checks guard resume (I4).
    IdentityWeak,
    /// The server stopped honoring `Range` mid-download (§9 Protocol);
    /// continuing on one connection.
    ServerIgnoredRange,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    StateChanged {
        id: DownloadId,
        from: State,
        to: State,
        reason: Option<FailReason>,
    },
    Progress {
        id: DownloadId,
        downloaded: u64,
        total: Option<u64>,
        connections: u32,
        /// 5-sample EMA of the 1 s samples (§14).
        speed_bps: u64,
        /// `None`: unknown size, under 3 samples or under 1 KiB/s.
        eta_secs: Option<u64>,
    },
    /// Started over from zero; the shell says why.
    Reset {
        id: DownloadId,
        reason: ResetReason,
    },
    /// Reconnecting after `failures` consecutive failures.
    Retrying {
        id: DownloadId,
        failures: u32,
        delay: Duration,
    },
    /// The file is at its final path (§8).
    Renamed {
        id: DownloadId,
        path: PathBuf,
    },
    Warning {
        id: DownloadId,
        kind: WarningKind,
    },
    /// The download's group is now `group` (§13.2).
    GroupChanged {
        id: DownloadId,
        group: Option<groups::GroupSnapshot>,
    },
}

pub struct AddRequest {
    pub url: Url,
    pub headers: Vec<Header>,
    /// Must exist.
    pub directory: PathBuf,
    /// User override; else the probe's suggestion (§3).
    pub filename: Option<String>,
    /// Record as paused instead of queued.
    pub paused: bool,
    /// Connections for this download; `None` = the default (§4.1).
    pub connections: Option<u8>,
    /// `None`: the default queue.
    pub queue: Option<queues::QueueId>,
    /// Bytes/s for this download; `None`: only its queue's and the global
    /// limit apply (§12).
    pub speed_limit: Option<u64>,
    pub target_fs: TargetFs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub id: DownloadId,
    pub url: String,
    pub final_path: PathBuf,
    pub part_path: PathBuf,
    pub state: State,
    pub fail_reason: Option<FailReason>,
    pub total_size: Option<u64>,
    /// Committed bytes per the sidecar (or the whole file once completed).
    pub downloaded: u64,
    pub resumable: Resumable,
    pub size_unverified: bool,
    pub identity_weak: bool,
    pub created_at: i64,
    pub queue_id: queues::QueueId,
    /// Order within its queue.
    pub position: i64,
    /// Bytes/s; `Some(0)`: paused by limit (§12).
    pub speed_limit: Option<u64>,
    /// Set at `Add` from the chosen name (§13.1).
    pub category: Category,
    pub group: Option<groups::GroupId>,
}

impl Snapshot {
    pub fn sidecar_path(&self) -> PathBuf {
        sidecar_for(&self.part_path)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Probe(#[from] ProbeError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("no download matches {0:?}")]
    NotFound(String),
    #[error("{0:?} matches more than one download")]
    Ambiguous(String),
    #[error("{0:?} is not a usable file name")]
    InvalidName(String),
    #[error("the default queue cannot be deleted")]
    DefaultQueue,
    #[error("{0}")]
    InvalidQueue(&'static str),
    #[error("{0}")]
    InvalidSetting(&'static str),
}

impl From<rusqlite::Error> for EngineError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Storage(e.into())
    }
}

pub struct Engine {
    db: Arc<Mutex<Db>>,
    client: http::ClientSlot,
    config: DownloadConfig,
    /// The connection budget, link knowledge and global speed limit all
    /// downloads in this process share (§10, §12).
    shared: Shared,
    buckets: Mutex<limits::Buckets>,
    speed: Mutex<speed::SpeedBook>,
    platform: Arc<dyn PlatformAdapter>,
    /// The adapter's last answer to `system_proxy`.
    last_system: Mutex<Option<ProxyConfig>>,
}

impl Engine {
    /// Opens (or creates) `dhow.sqlite` in `data_dir`, with no platform
    /// adapter.
    pub fn open(data_dir: &Path, config: DownloadConfig) -> Result<Self, EngineError> {
        Self::open_with(data_dir, config, Arc::new(NoopAdapter))
    }

    /// [`Engine::open`] with the shell's adapter (ARCHITECTURE §4).
    pub fn open_with(
        data_dir: &Path,
        config: DownloadConfig,
        platform: Arc<dyn PlatformAdapter>,
    ) -> Result<Self, EngineError> {
        let db = Db::open(&data_dir.join("dhow.sqlite"))?;
        let engine = Self {
            db: Arc::new(Mutex::new(db)),
            client: http::ClientSlot::default(),
            config,
            shared: Shared::new(config.budget),
            buckets: Mutex::default(),
            speed: Mutex::default(),
            platform,
            last_system: Mutex::default(),
        };
        engine.refresh_limits()?;
        // Leftovers of earlier runs whose hour has since ended.
        engine.roll_up(rollup_cutoff(now_ms()))?;
        Ok(engine)
    }

    /// The adapter's view of the network plus the proxy the HTTP client
    /// actually uses (ARCHITECTURE §8). Asks on every call; nothing is
    /// watched here.
    pub fn network_info(&self) -> Result<NetworkInfo, AdapterError> {
        let mut info = self.platform.network_info()?;
        info.proxy_in_use = self.proxy_in_use(info.system_proxy.as_ref());
        Ok(info)
    }

    fn db(&self) -> MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn speed(&self) -> MutexGuard<'_, speed::SpeedBook> {
        self.speed.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn buckets(&self) -> MutexGuard<'_, limits::Buckets> {
        self.buckets.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Probes and records a download (§2, §3) and groups it (§13.2).
    /// Doesn't start it. Fails fast as `Failed(NoSpace)` when the directory
    /// can't hold it (I10). Its group is in the snapshot; `GroupChanged`
    /// goes to the others it joined.
    pub async fn add(
        &self,
        req: AddRequest,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<Snapshot, EngineError> {
        let queue_id = req
            .queue
            .clone()
            .unwrap_or_else(|| DEFAULT_QUEUE_ID.to_owned());
        self.queue(&queue_id)?;
        let lease = self.lease(&req.url).await?;
        let info = probe::probe(&lease.client, &req.url, &req.headers, self.config.probe).await?;
        drop(lease);
        let base = match &req.filename {
            Some(name) => naming::sanitize(name, req.target_fs)
                .ok_or_else(|| EngineError::InvalidName(name.clone()))?,
            None => naming::base_name(
                info.content_disposition.as_deref(),
                info.content_type.as_deref(),
                &info.effective_url,
                &req.url,
                req.target_fs,
            ),
        };
        let mut taken: Vec<String> = std::fs::read_dir(&req.directory)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        taken.extend(self.reserved_names(&req.directory)?);
        // From the name before ` (n)` is added: that goes before what
        // naming calls the extension, which can be inside a longer one a
        // user added (`model (2).tar.lz`).
        let category = naming::categorize(
            &base,
            info.content_type.as_deref(),
            &self.extra_extensions()?,
        );
        let name = naming::deduplicate(&base, &taken);
        let final_path = req.directory.join(&name);
        let part_path = req.directory.join(format!("{name}.part"));

        let no_space = match info.total_size {
            Some(total) => fs4::available_space(&req.directory)? < total,
            None => false,
        };
        let (state, reason) = if no_space {
            (State::Failed, Some(FailReason::NoSpace))
        } else if req.paused {
            (State::Paused, None)
        } else {
            (State::Queued, None)
        };
        let id = uuid::Uuid::now_v7().to_string();
        let headers: Vec<(&str, &str)> = req
            .headers
            .iter()
            .map(|h| (h.name.as_str(), h.value.as_str()))
            .collect();
        let db = self.db();
        let position: i64 = db.conn().query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM downloads WHERE queue_id = ?1",
            [&queue_id],
            |r| r.get(0),
        )?;
        db.conn().execute(
            "INSERT INTO downloads (id, url, headers, final_path, part_path, state, fail_reason,
                 queue_id, position, category, created_at, total_size, etag, last_modified,
                 digest, resumable, connections, speed_limit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                 ?17, ?18)",
            params![
                id,
                req.url.as_str(),
                serde_json::to_string(&headers).expect("strings serialize"),
                path_str(&final_path),
                path_str(&part_path),
                state.as_str(),
                reason.as_ref().map(FailReason::as_db),
                queue_id,
                position,
                category.as_str(),
                now_ms(),
                info.total_size.map(|t| t as i64),
                info.etag,
                info.last_modified,
                info.digest.as_ref().map(digest_text),
                resumable_str(info.resumable),
                req.connections.map(|n| n.max(1)),
                req.speed_limit.map(|r| r as i64),
            ],
        )?;
        drop(db);
        // A download that can't be grouped is still added.
        let grouped = self.auto_group(&id).and_then(|changed| {
            let others: Vec<DownloadId> = changed.into_iter().filter(|c| *c != id).collect();
            self.announce_groups(&others, on_event)
        });
        if let Err(e) = grouped {
            tracing::warn!(id, error = %e, "could not group download");
        }
        self.stored(&id)
    }

    /// Final names of unfinished downloads in `dir`: their files don't
    /// exist yet, but the names are spoken for.
    fn reserved_names(&self, dir: &Path) -> Result<Vec<String>, EngineError> {
        let db = self.db();
        let mut stmt = db.conn().prepare(
            "SELECT final_path FROM downloads WHERE state NOT IN ('completed', 'cancelled')",
        )?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .map(PathBuf::from)
            .filter(|p| p.parent() == Some(dir))
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        Ok(names)
    }

    /// A completed download whose file moved is found again through its
    /// token first (ARCHITECTURE §6): `Renamed`, then any `GroupChanged`.
    pub fn get(
        &self,
        id: &str,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<Snapshot, EngineError> {
        self.located(self.stored(id)?, on_event)
    }

    /// Oldest first; moved files are found as in [`Engine::get`].
    pub fn list(
        &self,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<Vec<Snapshot>, EngineError> {
        let rows = self.rows()?;
        let mut moved = false;
        for snap in &rows {
            moved |= self.located(snap.clone(), on_event)?.final_path != snap.final_path;
        }
        // Regrouping a moved one can change rows already read.
        if moved { self.rows() } else { Ok(rows) }
    }

    fn rows(&self) -> Result<Vec<Snapshot>, EngineError> {
        let db = self.db();
        let mut stmt = db
            .conn()
            .prepare(&format!("{SELECT} ORDER BY position, created_at"))?;
        let rows = stmt.query_map([], snapshot)?.collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// The row as stored, without looking for a moved file.
    fn stored(&self, id: &str) -> Result<Snapshot, EngineError> {
        let db = self.db();
        let snap = db
            .conn()
            .query_row(&format!("{SELECT} WHERE id = ?1"), [id], snapshot)
            .optional()?;
        snap.ok_or_else(|| EngineError::NotFound(id.to_owned()))
    }

    /// The download whose id is `prefix` or starts with it.
    pub fn resolve(&self, prefix: &str) -> Result<DownloadId, EngineError> {
        let db = self.db();
        let mut stmt = db
            .conn()
            .prepare("SELECT id FROM downloads WHERE substr(id, 1, length(?1)) = ?1 LIMIT 2")?;
        let ids: Vec<String> = stmt
            .query_map([prefix], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        match ids.as_slice() {
            [id] => Ok(id.clone()),
            [] => Err(EngineError::NotFound(prefix.to_owned())),
            _ => Err(EngineError::Ambiguous(prefix.to_owned())),
        }
    }

    /// Marks a download that is not running as paused.
    pub fn pause(
        &self,
        id: &str,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<Snapshot, EngineError> {
        let snap = self.stored(id)?;
        if !snap.state.is_final() && snap.state != State::Paused && snap.state != State::Completing
        {
            self.transition(id, State::Paused, None, on_event)?;
        }
        self.stored(id)
    }

    /// Puts a download its runner stopped (schedule end, runner shutdown)
    /// back in its queue: `Paused` → `Queued`.
    pub fn requeue(
        &self,
        id: &str,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<Snapshot, EngineError> {
        if self.stored(id)?.state == State::Paused {
            self.transition(id, State::Queued, None, on_event)?;
        }
        self.stored(id)
    }

    /// Cancels a download that is not running; optionally deletes its
    /// partial files (never the final file).
    pub fn cancel(
        &self,
        id: &str,
        delete_partial: bool,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<Snapshot, EngineError> {
        let snap = self.stored(id)?;
        if snap.state.is_final() {
            return Ok(snap);
        }
        self.transition(id, State::Cancelled, None, on_event)?;
        if delete_partial {
            remove_if_exists(&snap.sidecar_path())?;
            remove_if_exists(&snap.part_path)?;
        }
        self.stored(id)
    }

    /// Runs a download in this task until it completes, fails, or `stop`
    /// turns `true` (then it is paused, everything received committed).
    pub async fn run(
        &self,
        id: &str,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
        stop: &watch::Receiver<bool>,
    ) -> Result<Snapshot, EngineError> {
        let snap = self.stored(id)?;
        if snap.state.is_final() {
            return Ok(snap);
        }
        let (part, sidecar) = (snap.part_path.clone(), snap.sidecar_path());
        // §8: died between rename and sidecar removal. The rename was the
        // commit point; finish the bookkeeping.
        if snap.state == State::Completing && complete::recover(&part, &sidecar, &snap.final_path)?
        {
            let token = tokens::create(self.platform.as_ref(), id, &snap.final_path);
            self.store_token(id, token);
            self.transition(id, State::Completed, None, on_event)?;
            on_event(EngineEvent::Renamed {
                id: id.to_owned(),
                path: snap.final_path.clone(),
            });
            return self.stored(id);
        }
        // Before it is Active: a failure here leaves it as it was.
        let url = Url::parse(&snap.url).map_err(io::Error::other)?;
        let lease = self.lease(&url).await?;
        self.transition(id, State::Active, None, on_event)?;

        let req = Request {
            url,
            headers: self.headers(id)?,
            part: part.clone(),
            sidecar: sidecar.clone(),
            connections: self.connections(id)?,
            started_at: self.started_at(id)?,
            limits: self.take_buckets(id, &snap.queue_id)?,
        };
        let progress = Mutex::new(Progress::new(snap.total_size));
        let dl_event = |e: DownloadEvent| self.on_download_event(id, e, &progress, on_event);
        let fetch = download::download(
            &req,
            &lease.client,
            &self.config,
            &self.shared,
            &dl_event,
            stop,
        );
        // Once a second while it runs, and only then (§14, §3.3).
        let sampler = async {
            let mut tick =
                tokio::time::interval_at(tokio::time::Instant::now() + SAMPLE_EVERY, SAMPLE_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                self.sample(id, &progress, on_event);
            }
        };
        let sampling = self.sampling();
        let result = tokio::select! {
            r = fetch => r,
            _ = sampler => unreachable!("samples forever"),
        };
        drop(sampling);
        self.drop_buckets(id);
        let done = match result {
            Ok(done) => done,
            Err(DownloadError::Stopped) => {
                self.transition(id, State::Paused, None, on_event)?;
                return self.stored(id);
            }
            Err(e) => {
                tracing::warn!(id, error = %e, "download failed");
                self.transition(id, State::Failed, Some(fail_reason(&e)), on_event)?;
                return self.stored(id);
            }
        };
        progress.lock().unwrap().flush(id, on_event);
        {
            let db = self.db();
            db.conn().execute(
                "UPDATE downloads SET total_size = ?2, etag = ?3, last_modified = ?4, digest = ?5,
                     resumable = ?6, size_unverified = ?7, identity_weak = ?8 WHERE id = ?1",
                params![
                    id,
                    done.info.total_size.map(|t| t as i64),
                    done.info.etag,
                    done.info.last_modified,
                    done.info.digest.as_ref().map(digest_text),
                    resumable_str(done.info.resumable),
                    done.size_unverified,
                    done.identity_weak,
                ],
            )?;
        }
        if done.size_unverified {
            on_event(EngineEvent::Warning {
                id: id.to_owned(),
                kind: WarningKind::SizeUnverified,
            });
        }
        if done.identity_weak {
            on_event(EngineEvent::Warning {
                id: id.to_owned(),
                kind: WarningKind::IdentityWeak,
            });
        }

        self.transition(id, State::Completing, None, on_event)?;
        let db = self.db.clone();
        let (directory, name) = split_final(&snap.final_path)?;
        let digest = done.info.digest.clone();
        let owned_id = id.to_owned();
        let platform = self.platform.clone();
        let finalized = tokio::task::spawn_blocking(move || {
            let target = Target {
                part: &part,
                sidecar: &sidecar,
                directory: &directory,
                name: &name,
            };
            complete::finalize(&target, digest.as_ref(), |path| {
                // Recorded before the rename so recovery knows the name.
                let db = db.lock().unwrap_or_else(|e| e.into_inner());
                db.conn()
                    .execute(
                        "UPDATE downloads SET final_path = ?2 WHERE id = ?1",
                        params![owned_id, path_str(path)],
                    )
                    .map_err(io::Error::other)?;
                Ok(())
            })
            .map(|path| {
                let token = tokens::create(platform.as_ref(), &owned_id, &path);
                (path, token)
            })
        })
        .await
        .map_err(io::Error::other)?;
        match finalized {
            Ok((path, token)) => {
                self.store_token(id, token);
                self.transition(id, State::Completed, None, on_event)?;
                on_event(EngineEvent::Renamed {
                    id: id.to_owned(),
                    path,
                });
            }
            Err(CompleteError::DigestMismatch { algorithm, .. }) => {
                tracing::warn!(id, ?algorithm, "digest mismatch");
                self.transition(
                    id,
                    State::Failed,
                    Some(FailReason::DigestMismatch),
                    on_event,
                )?;
            }
            Err(CompleteError::Io(e)) => {
                tracing::warn!(id, error = %e, "completion failed");
                self.transition(id, State::Failed, Some(FailReason::Io), on_event)?;
            }
        }
        self.stored(id)
    }

    fn sample(
        &self,
        id: &str,
        progress: &Mutex<Progress>,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) {
        let bps = {
            let mut p = progress.lock().unwrap();
            let bps = p.take_sample();
            p.flush(id, on_event);
            bps
        };
        if let Err(e) = self.record_sample(id, now_ms(), bps) {
            tracing::warn!(id, error = %e, "could not record a speed sample");
        }
    }

    fn on_download_event(
        &self,
        id: &str,
        e: DownloadEvent,
        progress: &Mutex<Progress>,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) {
        let mut p = progress.lock().unwrap();
        match e {
            DownloadEvent::Probed(info) => p.total = info.total_size,
            DownloadEvent::Resumed { committed } => {
                p.downloaded = committed;
                p.flush(id, on_event);
            }
            DownloadEvent::Reset(reason) => {
                p.downloaded = 0;
                on_event(EngineEvent::Reset {
                    id: id.to_owned(),
                    reason,
                });
                p.flush(id, on_event);
            }
            DownloadEvent::Connection(ConnEvent::Received(n)) => {
                p.downloaded += n;
                p.received += n;
                if p.last.elapsed() >= PROGRESS_EVERY {
                    p.flush(id, on_event);
                }
            }
            DownloadEvent::Connection(ConnEvent::Retrying {
                failures, delay, ..
            }) => {
                on_event(EngineEvent::Retrying {
                    id: id.to_owned(),
                    failures,
                    delay,
                });
            }
            DownloadEvent::Waiting(waiting) => {
                drop(p);
                let to = if waiting {
                    State::Waiting
                } else {
                    State::Active
                };
                if let Err(e) = self.transition(id, to, None, on_event) {
                    tracing::warn!(id, error = %e, "could not record budget wait");
                }
            }
            DownloadEvent::Connections(n) => {
                p.connections = n;
                p.flush(id, on_event);
            }
            DownloadEvent::Downgraded => {
                on_event(EngineEvent::Warning {
                    id: id.to_owned(),
                    kind: WarningKind::ServerIgnoredRange,
                });
            }
            DownloadEvent::Connection(ConnEvent::NotResumable) => {
                on_event(EngineEvent::Warning {
                    id: id.to_owned(),
                    kind: WarningKind::NotResumable,
                });
            }
        }
    }

    fn started_at(&self, id: &str) -> Result<Option<i64>, EngineError> {
        let db = self.db();
        Ok(db.conn().query_row(
            "SELECT started_at FROM downloads WHERE id = ?1",
            [id],
            |r| r.get(0),
        )?)
    }

    fn connections(&self, id: &str) -> Result<Option<u8>, EngineError> {
        let db = self.db();
        let n: Option<i64> = db.conn().query_row(
            "SELECT connections FROM downloads WHERE id = ?1",
            [id],
            |r| r.get(0),
        )?;
        Ok(n.and_then(|n| u8::try_from(n).ok()))
    }

    fn headers(&self, id: &str) -> Result<Vec<Header>, EngineError> {
        let db = self.db();
        let json: String =
            db.conn()
                .query_row("SELECT headers FROM downloads WHERE id = ?1", [id], |r| {
                    r.get(0)
                })?;
        let pairs: Vec<(String, String)> = serde_json::from_str(&json).map_err(io::Error::other)?;
        Ok(pairs
            .into_iter()
            .map(|(name, value)| Header { name, value })
            .collect())
    }

    /// Persists, then announces.
    fn transition(
        &self,
        id: &str,
        to: State,
        reason: Option<FailReason>,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<(), EngineError> {
        let from = {
            let db = self.db();
            let from: String =
                db.conn()
                    .query_row("SELECT state FROM downloads WHERE id = ?1", [id], |r| {
                        r.get(0)
                    })?;
            db.conn().execute(
                "UPDATE downloads SET state = ?2, fail_reason = ?3,
                     started_at = CASE WHEN ?2 = 'active' AND started_at IS NULL THEN ?4 ELSE started_at END,
                     completed_at = CASE WHEN ?2 = 'completed' THEN ?4 ELSE completed_at END
                 WHERE id = ?1",
                params![id, to.as_str(), reason.as_ref().map(FailReason::as_db), now_ms()],
            )?;
            State::parse(&from).unwrap_or(State::Failed)
        };
        on_event(EngineEvent::StateChanged {
            id: id.to_owned(),
            from,
            to,
            reason,
        });
        Ok(())
    }
}

struct Progress {
    downloaded: u64,
    total: Option<u64>,
    connections: u32,
    last: Instant,
    sent: Option<(u64, u32, u64, Option<u64>)>,
    /// Bytes received this run (unlike `downloaded`, not reset).
    received: u64,
    /// `received` and when at the last sample.
    sampled: (u64, Instant),
    ema: Ema,
}

impl Progress {
    fn new(total: Option<u64>) -> Self {
        Self {
            downloaded: 0,
            total,
            connections: 0,
            last: Instant::now(),
            sent: None,
            received: 0,
            sampled: (0, Instant::now()),
            ema: Ema::default(),
        }
    }

    /// Bytes/s since the last sample, into the EMA.
    fn take_sample(&mut self) -> u64 {
        let now = Instant::now();
        let (before, at) = self.sampled;
        let secs = now.duration_since(at).as_secs_f64();
        let bps = if secs > 0.0 {
            (self.received - before) as f64 / secs
        } else {
            0.0
        };
        self.sampled = (self.received, now);
        self.ema.push(bps);
        bps.round() as u64
    }

    fn flush(&mut self, id: &str, on_event: &(dyn Fn(EngineEvent) + Send + Sync)) {
        let speed_bps = self.ema.speed();
        let remaining = self.total.map(|t| t.saturating_sub(self.downloaded));
        let eta_secs = self.ema.eta(remaining);
        let now = (self.downloaded, self.connections, speed_bps, eta_secs);
        if self.sent == Some(now) {
            return;
        }
        self.last = Instant::now();
        self.sent = Some(now);
        on_event(EngineEvent::Progress {
            id: id.to_owned(),
            downloaded: self.downloaded,
            total: self.total,
            connections: self.connections,
            speed_bps,
            eta_secs,
        });
    }
}

fn fail_reason(e: &DownloadError) -> FailReason {
    match e {
        DownloadError::Probe(e) => match e {
            ProbeError::Status(code) => FailReason::Http(*code),
            ProbeError::TooManyRedirects => FailReason::TooManyRedirects,
            ProbeError::UnsupportedScheme(_) | ProbeError::InvalidHeader(_) => {
                FailReason::InvalidRequest
            }
            ProbeError::Timeout | ProbeError::Connect | ProbeError::Network => {
                FailReason::Transient
            }
        },
        DownloadError::Transfer(e) => match e {
            TransferError::Transient { .. } => FailReason::Transient,
            TransferError::Permanent(code) => FailReason::Http(*code),
            TransferError::TooManyRedirects => FailReason::TooManyRedirects,
            TransferError::InvalidHeader(_) => FailReason::InvalidRequest,
            TransferError::ServerSentExtraData => FailReason::ServerSentExtraData,
            TransferError::IdentityChanged(_) | TransferError::NeedsRestart => {
                FailReason::ServerKeptChanging
            }
            TransferError::Writer(_) => FailReason::Io,
        },
        DownloadError::Writer(WriterError::NoSpace { .. }) => FailReason::NoSpace,
        DownloadError::Writer(_) | DownloadError::Io(_) => FailReason::Io,
        DownloadError::TooManyResets(_) => FailReason::ServerKeptChanging,
        DownloadError::Stopped => unreachable!("handled by the caller"),
    }
}

const SELECT: &str = "SELECT id, url, final_path, part_path, state, fail_reason, total_size,
    resumable, size_unverified, identity_weak, created_at, queue_id, position, speed_limit,
    category, group_id FROM downloads";

fn snapshot(r: &Row) -> rusqlite::Result<Snapshot> {
    let state: String = r.get(4)?;
    let state = State::parse(&state).unwrap_or(State::Failed);
    let part_path = PathBuf::from(r.get::<_, String>(3)?);
    let final_path = PathBuf::from(r.get::<_, String>(2)?);
    let total_size = r.get::<_, Option<i64>>(6)?.map(|t| t as u64);
    let downloaded = match state {
        State::Completed => {
            std::fs::metadata(&final_path).map_or(total_size.unwrap_or(0), |m| m.len())
        }
        _ => std::fs::read(sidecar_for(&part_path))
            .ok()
            .and_then(|b| format::replay(&b).ok())
            .map_or(0, |r| r.committed.len()),
    };
    let resumable = match r.get::<_, String>(7)?.as_str() {
        "yes" => Resumable::Yes,
        "no" => Resumable::No,
        _ => Resumable::Unknown,
    };
    Ok(Snapshot {
        id: r.get(0)?,
        url: r.get(1)?,
        final_path,
        part_path,
        state,
        fail_reason: r
            .get::<_, Option<String>>(5)?
            .map(|s| FailReason::parse(&s)),
        total_size,
        downloaded,
        resumable,
        size_unverified: r.get(8)?,
        identity_weak: r.get(9)?,
        created_at: r.get(10)?,
        queue_id: r.get(11)?,
        position: r.get(12)?,
        speed_limit: r.get::<_, Option<i64>>(13)?.map(|v| v as u64),
        category: Category::parse(&r.get::<_, String>(14)?).unwrap_or(Category::Other),
        group: r.get(15)?,
    })
}

/// `<final>.part` → `<final>.dhow`.
fn sidecar_for(part: &Path) -> PathBuf {
    let s = part.as_os_str().to_string_lossy();
    PathBuf::from(format!("{}.dhow", s.strip_suffix(".part").unwrap_or(&s)))
}

fn split_final(path: &Path) -> io::Result<(PathBuf, String)> {
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => Ok((dir.to_owned(), name.to_string_lossy().into_owned())),
        _ => Err(io::Error::other(format!(
            "bad final path {}",
            path.display()
        ))),
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn resumable_str(r: Resumable) -> &'static str {
    match r {
        Resumable::Yes => "yes",
        Resumable::No => "no",
        Resumable::Unknown => "unknown",
    }
}

fn digest_text(d: &crate::probe::Digest) -> String {
    let hex: String = d.value.iter().map(|b| format!("{b:02x}")).collect();
    format!("{:?}:{hex}", d.algorithm).to_lowercase()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}
