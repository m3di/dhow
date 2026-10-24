mod lock;
mod output;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use dhow_core::download::DownloadConfig;
use dhow_core::engine::queues::{OnComplete, QueueSpec};
use dhow_core::engine::{AddRequest, Engine, EngineError, EngineEvent, Snapshot, State};
use dhow_core::http::{Header, Url};
use dhow_core::logging;
use dhow_core::naming::TargetFs;
use dhow_core::schedule::{Action, Schedule, Window};
use dhow_core::storage::Db;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use tokio::sync::{Notify, watch};

#[derive(Parser)]
#[command(name = "dhow", version = dhow_core::VERSION, about = "Move files between any two shores")]
struct Cli {
    /// Log directory [default: the platform's log directory]
    #[arg(long, env = "DHOW_LOG_DIR", global = true)]
    log_dir: Option<PathBuf>,

    /// Log filter, in `tracing` EnvFilter syntax
    #[arg(long, env = "DHOW_LOG", default_value = logging::DEFAULT_FILTER, global = true)]
    log: String,

    /// Database directory [default: the platform's data directory]
    #[arg(long, env = "DHOW_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,

    /// Consecutive failed attempts per connection before giving up
    #[arg(long, env = "DHOW_MAX_RETRIES", global = true, default_value_t = 10)]
    max_retries: u32,

    /// First retry delay in milliseconds (doubles each time, capped at 60 s)
    #[arg(
        long,
        env = "DHOW_RETRY_BASE_MS",
        global = true,
        default_value_t = 1000
    )]
    retry_base_ms: u64,

    /// Longest time received bytes stay uncommitted, in milliseconds
    #[arg(
        long,
        env = "DHOW_COMMIT_INTERVAL_MS",
        global = true,
        default_value_t = 2000
    )]
    commit_interval_ms: u64,

    /// Ramp-up measurement window in milliseconds (ENGINE-SPEC §4.4)
    #[arg(
        long,
        env = "DHOW_RAMP_WINDOW_MS",
        global = true,
        default_value_t = 3000
    )]
    ramp_window_ms: u64,

    /// Smallest chunk worth its own connection, in KiB (§4.1)
    #[arg(
        long,
        env = "DHOW_MIN_CHUNK_KIB",
        global = true,
        default_value_t = 1024
    )]
    min_chunk_kib: u64,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Add a download and run it here until it finishes
    Add {
        url: String,
        /// Directory to save into [default: current directory]
        #[arg(short, long)]
        dir: Option<PathBuf>,
        /// File name [default: from the server]
        #[arg(short, long)]
        output: Option<String>,
        /// Request header, `Name: value`; repeatable
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        /// Only add it; `dhow watch` runs it later
        #[arg(long, conflicts_with = "paused")]
        queue: bool,
        /// Add it paused
        #[arg(long)]
        paused: bool,
        /// Connections for this download [default: 8]
        #[arg(short = 'c', long)]
        connections: Option<u8>,
        /// Queue (name or id) [default: the default queue]
        #[arg(long)]
        to: Option<String>,
        /// Speed limit for this download, see `dhow limit`
        #[arg(long)]
        limit: Option<String>,
    },
    /// Run a paused, failed or interrupted download here until it finishes
    Resume { id: String },
    /// Pause a download (stops the process running it)
    Pause { id: String },
    /// Cancel a download
    Cancel {
        id: String,
        /// Also delete the .part and .dhow files
        #[arg(long)]
        delete_partial: bool,
    },
    /// List downloads, one JSON object per line
    Ls,
    /// Run queued downloads here, as many at once as their queues allow,
    /// until none are left
    Watch,
    /// Manage queues
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    /// Move a download to the end of another queue
    Move { id: String, queue: String },
    /// Move a download within its queue: before another, or to the end
    Reorder {
        id: String,
        #[arg(long)]
        before: Option<String>,
    },
    /// Limit a download's speed: bytes/s with an optional K, M or G
    /// (×1024), `0` to pause it by limit, `none` to lift it
    Limit { id: String, rate: String },
    /// Show or change the global speed limit and headroom
    Settings {
        /// Limit for all downloads together (see `dhow limit`)
        #[arg(long)]
        global_limit: Option<String>,
        /// Percent of the observed peak to leave free for everything else
        /// on the link (0–99)
        #[arg(long)]
        headroom: Option<u8>,
    },
}

#[derive(Subcommand)]
enum QueueCommand {
    /// Create a queue
    Add {
        name: String,
        /// Downloads it runs at once
        #[arg(long, default_value_t = 3)]
        max: u32,
        /// nothing, sleep, shutdown or hibernate
        #[arg(long, default_value = "nothing")]
        on_complete: String,
        /// Schedule window, `<days> <start>-<end>` in local time, e.g.
        /// `mon-fri 22:00-06:00`; repeatable
        #[arg(long = "window")]
        windows: Vec<String>,
        /// What the windows do: `start` (start inside, run on past the
        /// end) or `stop-outside` (also pause at the end)
        #[arg(long, default_value = "start")]
        action: String,
        /// Speed limit for the queue's downloads together, see `dhow limit`
        #[arg(long)]
        limit: Option<String>,
    },
    /// List queues
    Ls,
    /// Change a queue
    Set {
        queue: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        max: Option<u32>,
        #[arg(long)]
        on_complete: Option<String>,
        /// Replace the schedule's windows (see `queue add`)
        #[arg(long = "window")]
        windows: Vec<String>,
        #[arg(long)]
        action: Option<String>,
        /// Remove the schedule
        #[arg(long, conflicts_with = "windows")]
        no_schedule: bool,
        /// Speed limit (`none` lifts it), see `dhow limit`
        #[arg(long)]
        limit: Option<String>,
    },
    /// Start a queue now, whatever its schedule says, until its next edge
    Start { queue: String },
    /// Stop a queue (pausing what it runs) until its next schedule edge
    Stop { queue: String },
    /// Follow the schedule again
    Auto { queue: String },
    /// Delete a queue, moving its downloads to another
    Rm {
        queue: String,
        #[arg(long, default_value = "default")]
        move_to: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Not being able to log must not stop a download.
    match cli.log_dir.clone().or_else(logging::default_dir) {
        Some(dir) => {
            if let Err(e) = logging::init(&dir, &cli.log) {
                eprintln!("dhow: logging disabled: {e}");
            }
        }
        None => eprintln!("dhow: logging disabled: no log directory, pass --log-dir"),
    }
    tracing::info!(version = dhow_core::VERSION, "cli started");
    let Some(command) = cli.command else {
        return ExitCode::SUCCESS;
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fail(&e),
    };
    let mut config = DownloadConfig::default();
    config.transfer.retry.max_failures = cli.max_retries.max(1);
    config.transfer.retry.base = Duration::from_millis(cli.retry_base_ms);
    config.writer.commit_interval = Duration::from_millis(cli.commit_interval_ms.max(1));
    config.ramp.window = Duration::from_millis(cli.ramp_window_ms.max(1));
    config.ramp.min_chunk = cli.min_chunk_kib.max(1) * 1024;
    match runtime.block_on(run(command, &cli.data_dir, config)) {
        Ok(code) => code,
        Err(e) => fail(&*e),
    }
}

fn fail(e: &dyn std::error::Error) -> ExitCode {
    eprintln!("dhow: {e}");
    ExitCode::FAILURE
}

type Res<T> = Result<T, Box<dyn std::error::Error>>;

async fn run(
    command: Command,
    data_dir: &Option<PathBuf>,
    config: DownloadConfig,
) -> Res<ExitCode> {
    let data_dir = data_dir
        .clone()
        .or_else(Db::default_dir)
        .ok_or("no data directory; pass --data-dir")?;
    // Before any lock is taken: a lock's PID must always be safe to poke.
    let poke = poke_on_signal();
    let engine = Engine::open(&data_dir, config)?;
    let stop = stop_on_signal();
    let print = |e: &_| output::emit(output::event(e));
    let on_event = |e: dhow_core::engine::EngineEvent| print(&e);

    match command {
        Command::Add {
            url,
            dir,
            output: name,
            headers,
            queue,
            paused,
            connections,
            to,
            limit,
        } => {
            let target_queue = to.map(|q| engine.resolve_queue(&q)).transpose()?;
            let directory = std::path::absolute(dir.unwrap_or_else(|| PathBuf::from(".")))?;
            let req = AddRequest {
                url: Url::parse(&url)?,
                headers: headers
                    .iter()
                    .map(|h| parse_header(h))
                    .collect::<Res<_>>()?,
                directory,
                filename: name,
                paused,
                connections,
                queue: target_queue,
                speed_limit: limit.as_deref().map(parse_rate).transpose()?.flatten(),
                target_fs: TargetFs::host(),
            };
            let added = engine.add(req).await?;
            output::emit(output::snapshot("added", &added));
            if queue || added.state != State::Queued {
                lock::poke_runners(&data_dir);
                return Ok(exit_for(added.state));
            }
            runner(&engine, &data_dir, Some(added.id), &on_event, &stop, &poke).await
        }
        Command::Resume { id } => {
            let id = engine.resolve(&id)?;
            runner(&engine, &data_dir, Some(id), &on_event, &stop, &poke).await
        }
        Command::Pause { id } => {
            let id = engine.resolve(&id)?;
            // Record it, then make whoever runs it look.
            engine.pause(&id, &on_event)?;
            if let Some(pid) = lock::holder(&data_dir, &id)? {
                lock::stop_runner(&data_dir, &id, pid)?;
            }
            output::emit(output::snapshot("download", &engine.get(&id)?));
            Ok(ExitCode::SUCCESS)
        }
        Command::Cancel { id, delete_partial } => {
            let id = engine.resolve(&id)?;
            if let Some(pid) = lock::holder(&data_dir, &id)? {
                engine.pause(&id, &on_event)?;
                lock::stop_runner(&data_dir, &id, pid)?;
            }
            let _lock = match lock::acquire(&data_dir, &id) {
                Ok(l) => l,
                Err(lock::Held::By(pid)) => {
                    return Err(format!("download is running in process {pid}").into());
                }
                Err(lock::Held::Io(e)) => return Err(e.into()),
            };
            let snap = engine.cancel(&id, delete_partial, &on_event)?;
            output::emit(output::snapshot("download", &snap));
            Ok(ExitCode::SUCCESS)
        }
        Command::Ls => {
            for snap in engine.list()? {
                output::emit(output::snapshot("download", &snap));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Watch => runner(&engine, &data_dir, None, &on_event, &stop, &poke).await,
        Command::Queue { command } => {
            match command {
                QueueCommand::Add {
                    name,
                    max,
                    on_complete,
                    windows,
                    action,
                    limit,
                } => {
                    let spec = QueueSpec {
                        name,
                        max_concurrent: max,
                        speed_limit: limit.as_deref().map(parse_rate).transpose()?.flatten(),
                        schedule: parse_schedule(&windows, &action)?,
                        on_complete: parse_on_complete(&on_complete)?,
                    };
                    output::emit(output::queue(&engine.create_queue(&spec)?));
                    lock::poke_runners(&data_dir);
                }
                QueueCommand::Ls => {
                    for q in engine.list_queues()? {
                        output::emit(output::queue(&q));
                    }
                }
                QueueCommand::Set {
                    queue,
                    name,
                    max,
                    on_complete,
                    windows,
                    action,
                    no_schedule,
                    limit,
                } => {
                    let id = engine.resolve_queue(&queue)?;
                    let current = engine.queue(&id)?;
                    let schedule = if no_schedule {
                        None
                    } else if !windows.is_empty() {
                        let action = action.as_deref().unwrap_or("start");
                        parse_schedule(&windows, action)?
                    } else if let (Some(a), Some(mut s)) = (&action, current.schedule.clone()) {
                        s.action = parse_action(a)?;
                        Some(s)
                    } else {
                        current.schedule
                    };
                    let spec = QueueSpec {
                        name: name.or(current.name).unwrap_or_else(|| "default".into()),
                        max_concurrent: max.unwrap_or(current.max_concurrent),
                        speed_limit: match limit {
                            Some(v) => parse_rate(&v)?,
                            None => current.speed_limit,
                        },
                        schedule,
                        on_complete: match on_complete {
                            Some(v) => parse_on_complete(&v)?,
                            None => current.on_complete,
                        },
                    };
                    output::emit(output::queue(&engine.update_queue(&id, &spec)?));
                    lock::poke_runners(&data_dir);
                }
                QueueCommand::Start { queue } => {
                    let id = engine.resolve_queue(&queue)?;
                    output::emit(output::queue(&engine.start_queue(&id)?));
                    lock::poke_runners(&data_dir);
                }
                QueueCommand::Stop { queue } => {
                    let id = engine.resolve_queue(&queue)?;
                    output::emit(output::queue(&engine.stop_queue(&id)?));
                    lock::poke_runners(&data_dir);
                }
                QueueCommand::Auto { queue } => {
                    let id = engine.resolve_queue(&queue)?;
                    output::emit(output::queue(&engine.resume_schedule(&id)?));
                    lock::poke_runners(&data_dir);
                }
                QueueCommand::Rm { queue, move_to } => {
                    let id = engine.resolve_queue(&queue)?;
                    let to = engine.resolve_queue(&move_to)?;
                    engine.delete_queue(&id, &to)?;
                    lock::poke_runners(&data_dir);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Move { id, queue } => {
            let id = engine.resolve(&id)?;
            let queue = engine.resolve_queue(&queue)?;
            output::emit(output::snapshot(
                "download",
                &engine.move_to_queue(&id, &queue)?,
            ));
            lock::poke_runners(&data_dir);
            Ok(ExitCode::SUCCESS)
        }
        Command::Limit { id, rate } => {
            let id = engine.resolve(&id)?;
            let snap = engine.set_speed_limit(&id, parse_rate(&rate)?)?;
            output::emit(output::snapshot("download", &snap));
            lock::poke_runners(&data_dir);
            Ok(ExitCode::SUCCESS)
        }
        Command::Settings {
            global_limit,
            headroom,
        } => {
            if let Some(v) = global_limit {
                engine.set_global_limit(parse_rate(&v)?)?;
            }
            if let Some(p) = headroom {
                engine.set_headroom(p)?;
            }
            output::emit(output::settings(&engine.limit_settings()?));
            lock::poke_runners(&data_dir);
            Ok(ExitCode::SUCCESS)
        }
        Command::Reorder { id, before } => {
            let id = engine.resolve(&id)?;
            let before = before.map(|b| engine.resolve(&b)).transpose()?;
            output::emit(output::snapshot(
                "download",
                &engine.reorder(&id, before.as_deref())?,
            ));
            lock::poke_runners(&data_dir);
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Bytes/s: a number with an optional `K`, `M` or `G` (×1024, a trailing
/// `B` or `iB` allowed), or `none`.
fn parse_rate(v: &str) -> Res<Option<u64>> {
    let t = v.trim().to_ascii_lowercase();
    if matches!(t.as_str(), "none" | "off" | "unlimited") {
        return Ok(None);
    }
    let t = t
        .strip_suffix("ib")
        .or_else(|| t.strip_suffix('b'))
        .unwrap_or(&t);
    let (num, unit) = match t.char_indices().last() {
        Some((i, 'k')) => (&t[..i], 1u64 << 10),
        Some((i, 'm')) => (&t[..i], 1 << 20),
        Some((i, 'g')) => (&t[..i], 1 << 30),
        _ => (t, 1),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("{v:?} is not a rate (e.g. 500K, 2M, none)"))?;
    if !n.is_finite() || n < 0.0 {
        return Err(format!("{v:?} is not a rate (e.g. 500K, 2M, none)").into());
    }
    Ok(Some((n * unit as f64).round() as u64))
}

fn parse_action(v: &str) -> Res<Action> {
    match v {
        "start" => Ok(Action::Start),
        "stop-outside" | "start-and-stop-outside" => Ok(Action::StartAndStopOutside),
        _ => Err(format!("{v:?}: expected start or stop-outside").into()),
    }
}

fn parse_schedule(windows: &[String], action: &str) -> Res<Option<Schedule>> {
    if windows.is_empty() {
        return Ok(None);
    }
    let windows = windows
        .iter()
        .map(|w| {
            Window::parse(w).ok_or_else(|| {
                format!("{w:?}: expected `<days> HH:MM-HH:MM`, e.g. `mon-fri 22:00-06:00`")
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(Some(Schedule {
        windows,
        action: parse_action(action)?,
    }))
}

fn parse_on_complete(v: &str) -> Res<OnComplete> {
    OnComplete::parse(v)
        .ok_or_else(|| format!("{v:?}: expected nothing, sleep, shutdown or hibernate").into())
}

/// Runs downloads in this process: one (`add`, `resume`: started by
/// hand, so schedules don't apply), or with `None` whatever the queues say
/// (`watch`, ENGINE-SPEC §10.4, §11). Sleeps only until the next schedule
/// edge, and only when a schedule exists (ARCHITECTURE §7); a poke
/// (`SIGUSR1`) or a finishing download wakes it early.
async fn runner(
    engine: &Engine,
    data_dir: &Path,
    one: Option<String>,
    on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    stop: &watch::Receiver<bool>,
    poke: &Notify,
) -> Res<ExitCode> {
    let watching = one.is_none();
    let _registered = lock::register_runner(data_dir)?;
    let mut running = FuturesUnordered::new();
    // Per running download: its own stop signal, and whether to put it
    // back in its queue once stopped (stopped by the runner, not the user).
    let mut here: HashMap<String, (watch::Sender<bool>, bool)> = HashMap::new();
    let mut start = one;
    let mut stopping = false;
    let mut code = ExitCode::SUCCESS;
    loop {
        // Armed before the checks, so a poke during them still wakes us.
        let poked = poke.notified();
        tokio::pin!(poked);
        poked.as_mut().enable();
        // A limit another `dhow` changed (§12).
        engine.refresh_limits()?;

        if *stop.borrow() && !stopping {
            stopping = true;
            for (tx, requeue) in here.values_mut() {
                *requeue = watching;
                let _ = tx.send(true);
            }
        }
        // A pause or cancel another `dhow` recorded.
        for (id, (tx, _)) in &here {
            if matches!(engine.get(id)?.state, State::Paused | State::Cancelled) {
                let _ = tx.send(true);
            }
        }
        if watching && !stopping {
            let ids: Vec<String> = here.keys().cloned().collect();
            for id in engine.to_stop(ids.iter().map(String::as_str))? {
                if let Some((tx, requeue)) = here.get_mut(&id) {
                    *requeue = true;
                    let _ = tx.send(true);
                }
            }
            let next = engine.next_runnable(|id| {
                here.contains_key(id) || matches!(lock::holder(data_dir, id), Ok(Some(_)))
            })?;
            for snap in next {
                let Ok(lock) = lock::acquire(data_dir, &snap.id) else {
                    continue;
                };
                let (tx, rx) = watch::channel(false);
                here.insert(snap.id.clone(), (tx, false));
                running.push(run_locked(engine, lock, snap.id, on_event, rx));
            }
        }
        if let Some(id) = start.take() {
            let lock = match lock::acquire(data_dir, &id) {
                Ok(l) => l,
                Err(lock::Held::By(pid)) => {
                    return Err(format!("download is running in process {pid}").into());
                }
                Err(lock::Held::Io(e)) => return Err(e.into()),
            };
            let (tx, rx) = watch::channel(false);
            here.insert(id.clone(), (tx, false));
            running.push(run_locked(engine, lock, id, on_event, rx));
        }

        let edge = if watching && !stopping {
            engine.next_edge()?
        } else {
            None
        };
        if running.is_empty()
            && (!watching || stopping || edge.is_none() || !engine.has_waiting_work()?)
        {
            return Ok(code);
        }
        let wake = edge.map(deadline);
        tokio::select! {
            Some((id, result)) = running.next() => {
                let (_, requeue) = here.remove(&id).expect("running");
                match result {
                    Ok(snap) => {
                        // Stopped by its schedule or by stopping `watch`: it
                        // still belongs in its queue.
                        if requeue && snap.state == State::Paused {
                            engine.requeue(&id, on_event)?;
                        }
                        if exit_for(snap.state) != ExitCode::SUCCESS {
                            code = ExitCode::FAILURE;
                        }
                    }
                    Err(EngineError::NotFound(id)) => {
                        eprintln!("dhow: no download {id}");
                        code = ExitCode::FAILURE;
                    }
                    Err(e) => {
                        eprintln!("dhow: {id}: {e}");
                        code = ExitCode::FAILURE;
                    }
                }
            }
            _ = stopped(stop.clone()), if !stopping => {}
            _ = tokio::time::sleep_until(wake.unwrap_or_else(tokio::time::Instant::now)), if wake.is_some() => {}
            _ = &mut poked => {}
        }
    }
}

/// Runs a download whose lock this process holds.
async fn run_locked(
    engine: &Engine,
    _lock: lock::RunLock,
    id: String,
    on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    stop: watch::Receiver<bool>,
) -> (String, Result<Snapshot, EngineError>) {
    let result = engine.run(&id, on_event, &stop).await;
    (id, result)
}

/// The tokio instant of a wall-clock moment.
fn deadline(at: jiff::Timestamp) -> tokio::time::Instant {
    let wait = at.duration_since(jiff::Timestamp::now());
    tokio::time::Instant::now() + Duration::try_from(wait).unwrap_or_default()
}

/// Resolves once `stop` holds `true`; never if its sender is gone.
async fn stopped(mut stop: watch::Receiver<bool>) {
    if stop.wait_for(|s| *s).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// `SIGUSR1` ("re-check queues and downloads", from another `dhow`).
fn poke_on_signal() -> Arc<Notify> {
    let notify = Arc::new(Notify::new());
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut usr1) = signal(SignalKind::user_defined1()) {
            let n = notify.clone();
            tokio::spawn(async move {
                while usr1.recv().await.is_some() {
                    n.notify_waiters();
                }
            });
        }
    }
    notify
}

fn exit_for(state: State) -> ExitCode {
    match state {
        State::Failed | State::Cancelled => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    }
}

/// Ctrl-C (and SIGTERM, which `dhow pause` sends) pause instead of kill.
fn stop_on_signal() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let Ok(mut term) = signal(SignalKind::terminate()) else {
                return;
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        let _ = tx.send(true);
    });
    rx
}

fn parse_header(raw: &str) -> Res<Header> {
    let (name, value) = raw
        .split_once(':')
        .ok_or_else(|| format!("header {raw:?} is not `Name: value`"))?;
    Ok(Header {
        name: name.trim().to_owned(),
        value: value.trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_rate;

    #[test]
    fn rates() {
        assert_eq!(parse_rate("500").unwrap(), Some(500));
        assert_eq!(parse_rate("0").unwrap(), Some(0));
        assert_eq!(parse_rate("500K").unwrap(), Some(512_000));
        assert_eq!(parse_rate("1.5m").unwrap(), Some(1_572_864));
        assert_eq!(parse_rate("2MiB").unwrap(), Some(2 << 20));
        assert_eq!(parse_rate("100kb").unwrap(), Some(102_400));
        assert_eq!(parse_rate("1G").unwrap(), Some(1 << 30));
        assert_eq!(parse_rate("none").unwrap(), None);
        assert!(parse_rate("fast").is_err());
        assert!(parse_rate("-1K").is_err());
        assert!(parse_rate("").is_err());
    }
}
