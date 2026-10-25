//! The `Dhow` object of ARCHITECTURE §1 for in-process shells: the core's
//! engine, the tokio runtime it runs on, and the runner that turns the
//! core's queue policy (`next_runnable`, `to_stop`, `next_edge`) into
//! running downloads, so shells don't each reimplement it (ARCHITECTURE
//! §9).
//!
//! Idle means idle (§7): the runner is not a task. A pass runs on the
//! thread of whatever changed something (a command, a run ending) and
//! starts or stops runs; only while a schedule edge can start or stop
//! waiting work does one task sleep until it. Runs take the run lock
//! (`dhow_core::runlock`, without a PID) so the CLI never runs the same
//! download at once.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use dhow_core::engine::queues::RunState as RunStateCore;
use dhow_core::engine::{Engine as CoreEngine, EngineError, EngineEvent, Snapshot, State};
use dhow_core::platform::NoopAdapter;
use dhow_core::runlock::{self, Held};
use tokio::runtime::Runtime;
use tokio::sync::{Notify, watch};

use crate::adapter::{Bridge, PlatformAdapter};
use crate::error::DhowError;
use crate::events::{EventListener, Sink};
use crate::types::*;

/// How long dropping the engine waits for file work in flight (a
/// completion's digest check, a rename) before leaving it to finish alone.
const DROP_WAIT: Duration = Duration::from_secs(5);

/// One engine per data directory and process. Commands and queries may be
/// called from any thread; queries (and the commands that aren't `async`)
/// block on the database and the adapter, so shells call them off the main
/// thread.
///
/// Call `shutdown` before letting go of it. Dropping it without stops
/// everything at once (runs left `Active` are resumed by the next engine,
/// ENGINE-SPEC §5.4) and blocks the dropping thread up to 5 s for file work
/// in flight, so don't drop it on the main thread. The listener and the
/// adapter must hold it weakly, or it is never dropped.
#[derive(uniffi::Object)]
pub struct Engine {
    rt: Option<Runtime>,
    inner: Arc<Inner>,
}

struct Inner {
    core: CoreEngine,
    data_dir: PathBuf,
    sink: Sink,
    handle: tokio::runtime::Handle,
    runner: Mutex<Runner>,
    /// Wakes the schedule sleeper to look again.
    wake: Notify,
    /// The downloads the shell has heard of: each is announced once, also
    /// one another process added (and its removal).
    known: Mutex<HashSet<String>>,
}

#[derive(Default)]
struct Runner {
    runs: HashMap<String, Run>,
    /// Their run ended in an engine error that couldn't even be recorded
    /// as `Failed`: not started again by the queue until resumed (or the
    /// runner would retry it in a loop).
    failed: HashSet<String>,
    /// Being cancelled: not to be started meanwhile.
    cancelling: HashSet<String>,
    sleeper: bool,
    shut_down: bool,
}

struct Run {
    stop: watch::Sender<bool>,
    /// Closed when the run has ended and is out of `runs`.
    done: watch::Receiver<()>,
    /// Started by hand: its queue's schedule doesn't stop it (§11).
    by_hand: bool,
    /// A stop was sent.
    stopping: bool,
    /// Stopped by the runner, not the user: back to `Queued` once paused.
    requeue: bool,
    /// Resumed while stopping: started again, by hand, once it has ended.
    restart: bool,
}

impl Run {
    /// Stops it. It is requeued only if every stop asked for that: a
    /// user's pause wins over a schedule, `stop_queue` or `shutdown`. A
    /// stop also overrides an earlier resume.
    fn stop(&mut self, requeue: bool) -> watch::Receiver<()> {
        self.requeue = requeue && (self.requeue || !self.stopping);
        self.stopping = true;
        self.restart = false;
        let _ = self.stop.send(true);
        self.done.clone()
    }
}

async fn ended(mut done: watch::Receiver<()>) {
    while done.changed().await.is_ok() {}
}

#[uniffi::export]
impl Engine {
    /// Opens (or creates) the database in `data_dir` and starts what the
    /// queues say should run, including downloads a dead process left
    /// running. `adapter`: `None` for none (ARCHITECTURE §4).
    #[uniffi::constructor]
    pub fn open(
        data_dir: String,
        config: EngineConfig,
        listener: Arc<dyn EventListener>,
        adapter: Option<Arc<dyn PlatformAdapter>>,
    ) -> Result<Arc<Self>, DhowError> {
        let workers = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("dhow")
            .enable_all()
            .build()
            .map_err(io)?;
        let platform: Arc<dyn dhow_core::platform::PlatformAdapter> = match adapter {
            Some(a) => Arc::new(Bridge(a)),
            None => Arc::new(NoopAdapter),
        };
        let data_dir = PathBuf::from(data_dir);
        let core = {
            let _rt = rt.enter();
            CoreEngine::open_with(&data_dir, config.download_config(), platform)?
        };
        let known = core.ids()?.into_iter().collect();
        let inner = Arc::new(Inner {
            core,
            data_dir,
            sink: Sink::start(listener).map_err(io)?,
            handle: rt.handle().clone(),
            runner: Mutex::default(),
            wake: Notify::new(),
            known: Mutex::new(known),
        });
        let engine = Arc::new(Self {
            rt: Some(rt),
            inner,
        });
        engine.inner.kick();
        Ok(engine)
    }

    /// What `add` would learn (ENGINE-SPEC §2), for the add dialog;
    /// records nothing. `target_fs` as the `add` will have it, so the name
    /// matches. Fails with `Probe`, after which `add` with `blind` can
    /// still add it.
    #[uniffi::method(default(headers = [], target_fs = None))]
    pub async fn probe(
        &self,
        url: String,
        headers: Vec<Header>,
        target_fs: Option<TargetFs>,
    ) -> Result<ProbeResult, DhowError> {
        let url = parse_url(&url)?;
        let (headers, fs) = (core_headers(headers), core_fs(target_fs));
        let inner = self.inner.clone();
        self.spawn(async move { Ok(inner.core.probe(&url, &headers, fs).await?.into()) })
            .await
    }

    /// Probes and records a download (ENGINE-SPEC §2, §3), then starts it
    /// as `start` says. The snapshot is as recorded (and in
    /// `DownloadAdded`); `StateChanged` events follow once it runs.
    pub async fn add(&self, request: AddRequest) -> Result<DownloadSnapshot, DhowError> {
        let inner = self.inner.clone();
        self.spawn(async move { inner.add(request).await }).await
    }

    /// A completed download whose file moved is found again first
    /// (ARCHITECTURE §6).
    pub fn get(&self, id: String) -> Result<DownloadSnapshot, DhowError> {
        self.sync(|i| Ok(i.core.get(&id, &i.emit())?.into()))
    }

    /// In queue order. Downloads another process added or removed since
    /// this engine last looked are announced (`DownloadAdded`, `Removed`).
    pub fn list(&self) -> Result<Vec<DownloadSnapshot>, DhowError> {
        self.sync(|i| Ok(i.list()?.into_iter().map(Into::into).collect()))
    }

    /// Forgets a download, in any state (UI-GUIDELINES §3, §6): stops it
    /// first if it runs. `delete_files` also deletes its file: the final
    /// file if completed, else the part file and sidecar. `Removed` once
    /// done.
    pub async fn remove(&self, id: String, delete_files: bool) -> Result<(), DhowError> {
        let inner = self.inner.clone();
        self.spawn(async move { inner.remove(&id, delete_files).await })
            .await
    }

    /// `Failed` → `Queued`, keeping what it has (ENGINE-SPEC §9); its
    /// queue starts it when it may. Others are left as they are.
    pub fn retry(&self, id: String) -> Result<DownloadSnapshot, DhowError> {
        self.sync(|i| i.retry(&id))
    }

    /// Its connection count (§4.1; `None`: the default). A running download
    /// follows at once: more through ramp-up (§4.4), fewer by stopping its
    /// newest connections.
    pub fn set_connections(
        &self,
        id: String,
        connections: Option<u8>,
    ) -> Result<DownloadSnapshot, DhowError> {
        self.sync(|i| Ok(i.core.set_connections(&id, connections)?.into()))
    }

    /// `RangeMap` events for the detail pane (ARCHITECTURE §3.3): on, one
    /// at once, then at most one a second while it runs, and one when its
    /// run ends. Turn it off when the pane hides; nothing runs for it
    /// otherwise.
    pub fn watch_ranges(&self, id: String, on: bool) -> Result<(), DhowError> {
        self.sync(|i| Ok(i.core.watch_ranges(&id, on, &i.emit())?))
    }

    /// Stops a running download (everything received is committed first)
    /// and returns once it is `Paused`, or marks one that isn't running.
    /// One finishing its completion completes instead.
    pub async fn pause(&self, id: String) -> Result<DownloadSnapshot, DhowError> {
        let inner = self.inner.clone();
        self.spawn(async move { inner.pause(&id).await }).await
    }

    /// Starts a download now, by hand: its queue's schedule doesn't apply,
    /// it counts toward the queue's concurrency (ENGINE-SPEC §11). Also
    /// retries a failed one from its journal. Returns before it runs.
    pub fn resume(&self, id: String) -> Result<DownloadSnapshot, DhowError> {
        self.sync(|i| i.resume(&id))
    }

    /// Stops it if it runs, then cancels it; `delete_partial` also removes
    /// the part file and sidecar (never a final file).
    pub async fn cancel(
        &self,
        id: String,
        delete_partial: bool,
    ) -> Result<DownloadSnapshot, DhowError> {
        let inner = self.inner.clone();
        self.spawn(async move { inner.cancel(&id, delete_partial).await })
            .await
    }

    /// Bytes/s; `Some(0)` pauses it by limit, `None` lifts its own limit
    /// (ENGINE-SPEC §12). Applies to a running download at once.
    pub fn set_speed_limit(
        &self,
        id: String,
        limit: Option<u64>,
    ) -> Result<DownloadSnapshot, DhowError> {
        self.sync(|i| Ok(i.core.set_speed_limit(&id, limit)?.into()))
    }

    /// To the end of `queue`.
    pub fn move_to_queue(&self, id: String, queue: String) -> Result<DownloadSnapshot, DhowError> {
        self.command(|i| {
            let from = i.core.queue_of(&id)?;
            let snap = i.core.move_to_queue(&id, &queue)?;
            i.queue_changed(&from.id);
            i.queue_changed(&queue);
            Ok(snap.into())
        })
    }

    /// Before `before` in the same queue, or last.
    pub fn reorder(
        &self,
        id: String,
        before: Option<String>,
    ) -> Result<DownloadSnapshot, DhowError> {
        self.command(|i| Ok(i.core.reorder(&id, before.as_deref())?.into()))
    }

    /// Default queue first, then by name.
    pub fn list_queues(&self) -> Result<Vec<QueueSnapshot>, DhowError> {
        self.sync(|i| Ok(i.core.list_queues()?.into_iter().map(Into::into).collect()))
    }

    pub fn get_queue(&self, id: String) -> Result<QueueSnapshot, DhowError> {
        self.sync(|i| Ok(i.core.queue(&id)?.into()))
    }

    pub fn create_queue(&self, spec: QueueSpec) -> Result<QueueSnapshot, DhowError> {
        self.command(|i| Ok(i.announce(i.core.create_queue(&spec.try_into()?)?)))
    }

    pub fn update_queue(&self, id: String, spec: QueueSpec) -> Result<QueueSnapshot, DhowError> {
        self.command(|i| Ok(i.announce(i.core.update_queue(&id, &spec.try_into()?)?)))
    }

    /// Appends its downloads to `move_items_to`; not the default queue.
    /// `QueueRemoved`.
    pub fn delete_queue(&self, id: String, move_items_to: String) -> Result<(), DhowError> {
        self.command(|i| {
            i.core.delete_queue(&id, &move_items_to)?;
            i.sink.push(Event::QueueRemoved { id });
            i.queue_changed(&move_items_to);
            Ok(())
        })
    }

    /// Runs the queue now, overriding its schedule until the next edge
    /// (ENGINE-SPEC §11).
    pub fn start_queue(&self, id: String) -> Result<QueueSnapshot, DhowError> {
        self.command(|i| Ok(i.announce(i.core.start_queue(&id)?)))
    }

    /// Starts nothing more from it and stops what of it runs, also those
    /// started by hand (back to `Queued`), until the next edge.
    pub fn stop_queue(&self, id: String) -> Result<QueueSnapshot, DhowError> {
        self.sync(|i| i.stop_queue(&id))
    }

    /// Ends a `start_queue`/`stop_queue` override.
    pub fn resume_schedule(&self, id: String) -> Result<QueueSnapshot, DhowError> {
        self.command(|i| Ok(i.announce(i.core.resume_schedule(&id)?)))
    }

    /// Oldest first.
    pub fn list_groups(&self) -> Result<Vec<GroupSnapshot>, DhowError> {
        self.sync(|i| Ok(i.core.list_groups()?.into_iter().map(Into::into).collect()))
    }

    pub fn get_group(&self, id: String) -> Result<GroupSnapshot, DhowError> {
        self.sync(|i| Ok(i.core.group(&id)?.into()))
    }

    /// Puts a download in `group` (`None`: out of any) by hand; auto-grouping
    /// leaves it alone from then on (ENGINE-SPEC §13.2).
    pub fn set_group(
        &self,
        id: String,
        group: Option<String>,
    ) -> Result<DownloadSnapshot, DhowError> {
        self.sync(|i| Ok(i.core.set_group(&id, group.as_deref(), &i.emit())?.into()))
    }

    /// Which makes it manual.
    pub fn rename_group(&self, group: String, name: String) -> Result<GroupSnapshot, DhowError> {
        self.sync(|i| Ok(i.core.rename_group(&group, &name, &i.emit())?.into()))
    }

    pub fn settings(&self) -> Result<DhowSettings, DhowError> {
        self.sync(|i| Ok(i.core.settings()?.into()))
    }

    /// Validated whole and stored in one transaction, or not at all; then
    /// `SettingsChanged` (ARCHITECTURE §3.1).
    pub fn set_settings(&self, patch: SettingsPatch) -> Result<DhowSettings, DhowError> {
        self.sync(|i| Ok(i.core.set_settings(patch.try_into()?, &i.emit())?.into()))
    }

    /// Completed downloads by category (ENGINE-SPEC §13.1).
    pub fn storage_breakdown(&self) -> Result<Vec<CategoryUsage>, DhowError> {
        self.sync(|i| {
            Ok(i.core
                .storage_breakdown(&i.emit())?
                .into_iter()
                .map(Into::into)
                .collect())
        })
    }

    /// A download's samples, or with `None` the global aggregate's, oldest
    /// first (ENGINE-SPEC §14).
    pub fn speed_history(&self, id: Option<String>) -> Result<Vec<SpeedSample>, DhowError> {
        self.sync(|i| {
            Ok(i.core
                .speed_history(id.as_deref())?
                .into_iter()
                .map(Into::into)
                .collect())
        })
    }

    /// Applies the queues and schedules again now, and announces what
    /// another process changed: downloads added or removed, and every
    /// queue (`QueueChanged`). The schedule's wake-up runs on a monotonic
    /// clock, which stops while the machine sleeps: shells call this on
    /// wake, when the clock or time zone changes (ENGINE-SPEC §11), and
    /// when the app becomes active. Off the main thread, like a query.
    pub fn recheck(&self) {
        let _rt = self.rt().enter();
        if let Err(e) = self.inner.list() {
            tracing::warn!(error = %e, "could not read the downloads");
        }
        match self.inner.core.list_queues() {
            Ok(queues) => queues.into_iter().for_each(|q| {
                self.inner.announce(q);
            }),
            Err(e) => tracing::warn!(error = %e, "could not read the queues"),
        }
        self.inner.kick();
    }

    /// Asks the adapter now (ARCHITECTURE §8).
    pub fn network_info(&self) -> Result<NetworkInfo, DhowError> {
        self.sync(|i| Ok(i.core.network_info()?.into()))
    }

    /// The shell's network monitor saw a change (M4.3): asks the adapter
    /// and emits `NetworkChanged`.
    pub fn network_changed(&self) -> Result<NetworkInfo, DhowError> {
        self.sync(|i| {
            let info: NetworkInfo = i.core.network_info()?.into();
            i.sink.push(Event::NetworkChanged { info: info.clone() });
            Ok(info)
        })
    }

    /// Stops every run cleanly (committed, back to `Queued` so the next
    /// engine continues them) and starts nothing more. Queries and
    /// commands that only record still work; starting fails with
    /// `ShutDown`.
    pub async fn shutdown(&self) {
        let inner = self.inner.clone();
        let _ = self
            .spawn(async move {
                inner.shutdown().await;
                Ok(())
            })
            .await;
    }
}

/// Rust-side only, not in the bindings.
impl Engine {
    /// Tasks alive on the engine's runtime (T-idle).
    #[doc(hidden)]
    pub fn alive_tasks(&self) -> usize {
        self.rt().metrics().num_alive_tasks()
    }

    fn rt(&self) -> &Runtime {
        self.rt.as_ref().expect("until dropped")
    }

    fn sync<T>(&self, f: impl FnOnce(&Arc<Inner>) -> Result<T, DhowError>) -> Result<T, DhowError> {
        let _rt = self.rt().enter();
        f(&self.inner)
    }

    /// A change that can make the queues start or stop something.
    fn command<T>(
        &self,
        f: impl FnOnce(&Arc<Inner>) -> Result<T, DhowError>,
    ) -> Result<T, DhowError> {
        let out = self.sync(f)?;
        self.inner.kick();
        Ok(out)
    }

    /// On the engine's runtime, whoever polls the call. Dropping the call
    /// doesn't stop it: a command, once begun, is carried out.
    fn spawn<T: Send + 'static>(
        &self,
        f: impl Future<Output = Result<T, DhowError>> + Send + 'static,
    ) -> impl Future<Output = Result<T, DhowError>> + Send + 'static {
        let task = self.rt().spawn(f);
        async move {
            task.await.unwrap_or_else(|e| {
                Err(DhowError::Internal {
                    message: e.to_string(),
                })
            })
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.inner.runner().shut_down = true;
        if let Some(rt) = self.rt.take() {
            // Can't block inside a runtime (the adapter or listener may
            // drop the last reference on one of its threads).
            if tokio::runtime::Handle::try_current().is_ok() {
                rt.shutdown_background();
            } else {
                rt.shutdown_timeout(DROP_WAIT);
            }
        }
        self.inner.sink.close();
    }
}

/// Which of a queue's counts a download in this state is in.
fn counted(s: State) -> u8 {
    match s {
        State::Queued => 0,
        State::Active | State::Waiting | State::Completing => 1,
        State::Completed | State::Cancelled => 2,
        State::Paused | State::Failed => 3,
    }
}

fn io(e: std::io::Error) -> DhowError {
    DhowError::Io {
        message: e.to_string(),
    }
}

impl Inner {
    fn runner(&self) -> MutexGuard<'_, Runner> {
        self.runner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn known(&self) -> MutexGuard<'_, HashSet<String>> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self) -> impl Fn(EngineEvent) + Send + Sync + '_ {
        move |e| self.publish(e)
    }

    /// To the shell, with the `QueueChanged` an event implies: a download
    /// added, removed, or moved between queued, running, finished and the
    /// rest. A download already announced isn't again (see `list`); one
    /// the shell hasn't heard of (another process added it) is announced
    /// before its first event.
    fn publish(&self, e: EngineEvent) {
        let queue = {
            // Held while pushing: announcements keep their order.
            let mut known = self.known();
            let queue = match &e {
                EngineEvent::DownloadAdded { snapshot } => {
                    if !known.insert(snapshot.id.clone()) {
                        return;
                    }
                    Some(self.core.queue(&snapshot.queue_id))
                }
                EngineEvent::Removed { id, queue_id } => {
                    if !known.remove(id) {
                        return;
                    }
                    Some(self.core.queue(queue_id))
                }
                EngineEvent::StateChanged { id, from, to, .. } => {
                    (counted(*from) != counted(*to)).then(|| self.core.queue_of(id))
                }
                _ => None,
            };
            let event: Event = e.into();
            if let Some(id) = event.download()
                && !known.contains(id)
                && let Ok(snap) = self.core.row(id)
            {
                known.insert(snap.id.clone());
                self.sink.push(Event::DownloadAdded {
                    snapshot: snap.into(),
                });
            }
            self.sink.push(event);
            queue
        };
        match queue {
            Some(Ok(q)) => {
                self.announce(q);
            }
            Some(Err(e)) => tracing::warn!(error = %e, "could not read a queue"),
            None => {}
        }
    }

    /// `QueueChanged`, and the snapshot for the caller.
    fn announce(&self, q: dhow_core::engine::queues::QueueSnapshot) -> QueueSnapshot {
        let snapshot: QueueSnapshot = q.into();
        self.sink.push(Event::QueueChanged {
            snapshot: snapshot.clone(),
        });
        snapshot
    }

    fn queue_changed(&self, id: &str) {
        match self.core.queue(id) {
            Ok(q) => {
                self.announce(q);
            }
            Err(e) => tracing::warn!(error = %e, "could not read a queue"),
        }
    }

    /// Every download, announcing those another process added or removed
    /// since the last look (their queues' counts changed too).
    fn list(&self) -> Result<Vec<dhow_core::engine::Snapshot>, DhowError> {
        let rows = {
            // Held throughout, so an `add` or `remove` here announces
            // either through its event or through this, not both, and
            // what is announced here comes before any later event of it.
            let mut known = self.known();
            // What listing emits (`Renamed`, `GroupChanged`) goes straight
            // out: `publish` would wait for `known`.
            let rows = self.core.list(&|e| self.sink.push(e.into()))?;
            let ids: HashSet<&str> = rows.iter().map(|s| s.id.as_str()).collect();
            let removed: Vec<String> = known
                .iter()
                .filter(|k| !ids.contains(k.as_str()))
                .cloned()
                .collect();
            for id in &removed {
                known.remove(id);
            }
            let added: Vec<Snapshot> = rows
                .iter()
                .filter(|s| known.insert(s.id.clone()))
                .cloned()
                .collect();
            if added.is_empty() && removed.is_empty() {
                return Ok(rows);
            }
            for s in added {
                self.sink.push(Event::DownloadAdded { snapshot: s.into() });
            }
            for id in removed {
                self.sink.push(Event::Removed { id });
            }
            rows
        };
        for q in self.core.list_queues()? {
            self.announce(q);
        }
        Ok(rows)
    }

    fn held_elsewhere(&self, id: &str) -> bool {
        matches!(runlock::holder(&self.data_dir, id), Ok(Some(_)))
    }

    async fn add(self: &Arc<Self>, request: AddRequest) -> Result<DownloadSnapshot, DhowError> {
        if self.runner().shut_down {
            return Err(DhowError::ShutDown);
        }
        let start = request.start;
        let snap = self.core.add(request.into_core()?, &self.emit()).await?;
        // Recorded: from here on it is `Ok`, whatever starting it does.
        if snap.state == State::Queued {
            let mut r = self.runner();
            if start == StartMode::Now
                && let Err(e) = self.start(&mut r, &snap.id, true)
            {
                tracing::warn!(id = snap.id, error = %e, "could not start an added download");
            }
            self.pass(&mut r);
        }
        Ok(snap.into())
    }

    async fn pause(self: &Arc<Self>, id: &str) -> Result<DownloadSnapshot, DhowError> {
        let running = {
            let mut r = self.runner();
            match r.runs.get_mut(id) {
                Some(run) => Some(run.stop(false)),
                None if self.held_elsewhere(id) => {
                    return Err(DhowError::RunningElsewhere { id: id.to_owned() });
                }
                // Under the runner's lock: no pass starts it meanwhile.
                None => return Ok(self.core.pause(id, &self.emit())?.into()),
            }
        };
        if let Some(done) = running {
            ended(done).await;
        }
        Ok(self.core.get(id, &self.emit())?.into())
    }

    fn resume(self: &Arc<Self>, id: &str) -> Result<DownloadSnapshot, DhowError> {
        // Not under the runner's lock: `get` may ask the adapter (§6).
        let snap = self.core.get(id, &self.emit())?;
        if !snap.state.is_final() {
            let mut r = self.runner();
            if !r.cancelling.contains(id) {
                self.start(&mut r, id, true)?;
                self.pass(&mut r);
            }
        }
        Ok(snap.into())
    }

    async fn cancel(
        self: &Arc<Self>,
        id: &str,
        delete_partial: bool,
    ) -> Result<DownloadSnapshot, DhowError> {
        self.stopped(id, async {
            Ok(self
                .core
                .cancel(id, delete_partial, &self.emit())
                .await?
                .into())
        })
        .await
    }

    async fn remove(self: &Arc<Self>, id: &str, delete_files: bool) -> Result<(), DhowError> {
        self.stopped(id, async {
            Ok(self.core.remove(id, delete_files, &self.emit()).await?)
        })
        .await
    }

    /// Stops `id` if it runs and keeps it from starting, then does `f`
    /// holding its run lock: like a run, no other process may touch its
    /// files meanwhile.
    async fn stopped<T>(
        self: &Arc<Self>,
        id: &str,
        f: impl Future<Output = Result<T, DhowError>>,
    ) -> Result<T, DhowError> {
        self.core.get(id, &self.emit())?;
        let running = {
            let mut r = self.runner();
            r.cancelling.insert(id.to_owned());
            r.runs.get_mut(id).map(|run| run.stop(false))
        };
        if let Some(done) = running {
            ended(done).await;
        }
        let result = async {
            let _lock = runlock::acquire(&self.data_dir, id, None).map_err(|e| match e {
                Held::By(_) => DhowError::RunningElsewhere { id: id.to_owned() },
                Held::Io(e) => io(e),
            })?;
            f.await
        }
        .await;
        let mut r = self.runner();
        r.cancelling.remove(id);
        r.failed.remove(id);
        result
    }

    fn retry(self: &Arc<Self>, id: &str) -> Result<DownloadSnapshot, DhowError> {
        let snap = self.core.retry(id, &self.emit())?;
        let mut r = self.runner();
        r.failed.remove(id);
        self.pass(&mut r);
        Ok(snap.into())
    }

    fn stop_queue(self: &Arc<Self>, id: &str) -> Result<QueueSnapshot, DhowError> {
        let q = self.core.stop_queue(id)?;
        let running: Vec<String> = self.runner().runs.keys().cloned().collect();
        let in_queue: Vec<String> = running
            .into_iter()
            .filter(|i| {
                self.core
                    .get(i, &self.emit())
                    .is_ok_and(|s| s.queue_id == q.id)
            })
            .collect();
        let mut r = self.runner();
        for i in in_queue {
            if let Some(run) = r.runs.get_mut(&i) {
                run.stop(true);
            }
        }
        self.pass(&mut r);
        Ok(self.announce(q))
    }

    async fn shutdown(self: &Arc<Self>) {
        let runs: Vec<_> = {
            let mut r = self.runner();
            r.shut_down = true;
            r.runs.values_mut().map(|run| run.stop(true)).collect()
        };
        self.wake.notify_one();
        for done in runs {
            ended(done).await;
        }
    }

    /// Starts `id` in this process. One running already counts as started
    /// by hand if `by_hand`; one stopping is started again once it ends.
    fn start(self: &Arc<Self>, r: &mut Runner, id: &str, by_hand: bool) -> Result<(), DhowError> {
        if r.shut_down {
            return Err(DhowError::ShutDown);
        }
        if let Some(run) = r.runs.get_mut(id) {
            if !run.stopping {
                run.by_hand |= by_hand;
            } else if by_hand {
                // By hand from now: the schedule's next pass leaves it be.
                run.by_hand = true;
                run.restart = true;
            }
            return Ok(());
        }
        let lock = runlock::acquire(&self.data_dir, id, None).map_err(|e| match e {
            Held::By(_) => DhowError::RunningElsewhere { id: id.to_owned() },
            Held::Io(e) => io(e),
        })?;
        let (stop, stop_rx) = watch::channel(false);
        let (done_tx, done) = watch::channel(());
        r.failed.remove(id);
        r.runs.insert(
            id.to_owned(),
            Run {
                stop,
                done,
                by_hand,
                stopping: false,
                requeue: false,
                restart: false,
            },
        );
        let inner = self.clone();
        let id = id.to_owned();
        self.handle.spawn(async move {
            let result = inner.core.run(&id, &inner.emit(), &stop_rx).await;
            drop(lock);
            inner.finished(&id, result);
            drop(done_tx);
        });
        Ok(())
    }

    fn finished(self: &Arc<Self>, id: &str, result: Result<Snapshot, EngineError>) {
        let mut r = self.runner();
        let (requeue, restart) = r
            .runs
            .remove(id)
            .map_or((false, false), |run| (run.requeue, run.restart));
        match result {
            Ok(snap) if restart && !snap.state.is_final() => {
                if let Err(e) = self.start(&mut r, id, true) {
                    tracing::warn!(id, error = %e, "could not restart a resumed download");
                }
            }
            Ok(snap) if requeue && snap.state == State::Paused => {
                if let Err(e) = self.core.requeue(id, &self.emit()) {
                    tracing::warn!(id, error = %e, "could not requeue a stopped download");
                }
            }
            Ok(_) => {}
            // ENGINE-SPEC §9 Local: recorded, so it shows and frees its slot.
            Err(e) => {
                tracing::warn!(id, error = %e, "could not run download");
                if let Err(e) = self.core.fail(id, &self.emit()) {
                    tracing::warn!(id, error = %e, "could not record a failed run");
                    r.failed.insert(id.to_owned());
                }
            }
        }
        // Its slot is free.
        self.pass(&mut r);
    }

    fn kick(self: &Arc<Self>) {
        let mut r = self.runner();
        self.pass(&mut r);
    }

    /// Applies the queue policy now, then makes sure the next schedule
    /// edge wakes a pass if one can matter.
    fn pass(self: &Arc<Self>, r: &mut Runner) {
        self.pass_once(r);
        if r.sleeper {
            self.wake.notify_one();
        } else if self.edge(r).is_some() {
            r.sleeper = true;
            self.handle.spawn(self.clone().sleep());
        }
    }

    fn pass_once(self: &Arc<Self>, r: &mut Runner) {
        if r.shut_down {
            return;
        }
        let scheduled: Vec<String> = r
            .runs
            .iter()
            .filter(|(_, run)| !run.by_hand)
            .map(|(id, _)| id.clone())
            .collect();
        match self.core.to_stop(scheduled.iter().map(String::as_str)) {
            Ok(ids) => {
                for id in ids {
                    if let Some(run) = r.runs.get_mut(&id) {
                        run.stop(true);
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not check schedules"),
        }
        let next = self.core.next_runnable(|id| {
            r.runs.contains_key(id)
                || r.failed.contains(id)
                || r.cancelling.contains(id)
                || self.held_elsewhere(id)
        });
        let next = match next {
            Ok(next) => next,
            Err(e) => {
                tracing::warn!(error = %e, "could not read the queues");
                return;
            }
        };
        for snap in next {
            match self.start(r, &snap.id, false) {
                Ok(()) | Err(DhowError::RunningElsewhere { .. }) => {}
                Err(e) => tracing::warn!(id = snap.id, error = %e, "could not start download"),
            }
        }
    }

    /// The next schedule edge, if work waits that it could start or stop.
    fn edge(&self, r: &Runner) -> Option<jiff::Timestamp> {
        if r.shut_down {
            return None;
        }
        let edge = self.core.next_edge().ok().flatten()?;
        self.core.has_waiting_work().ok()?.then_some(edge)
    }

    /// `QueueChanged` for the queues an edge can have changed: those with
    /// a schedule or an override.
    fn edge_passed(&self) {
        match self.core.list_queues() {
            Ok(queues) => {
                for q in queues {
                    if q.schedule.is_some() || q.run_state != RunStateCore::Auto {
                        self.announce(q);
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not read the queues"),
        }
    }

    /// The single `sleep_until(next_edge)` of ARCHITECTURE §7; gone once no
    /// edge can matter.
    async fn sleep(self: Arc<Self>) {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let edge = {
                let mut r = self.runner();
                match self.edge(&r) {
                    Some(edge) => edge,
                    None => {
                        r.sleeper = false;
                        return;
                    }
                }
            };
            let wait = edge.duration_since(jiff::Timestamp::now());
            let wait = Duration::try_from(wait).unwrap_or_default();
            tokio::select! {
                _ = tokio::time::sleep(wait) => {
                    let mut r = self.runner();
                    self.pass_once(&mut r);
                    self.edge_passed();
                }
                _ = &mut notified => {}
            }
        }
    }
}
