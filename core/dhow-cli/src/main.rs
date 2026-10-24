mod lock;
mod output;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use dhow_core::download::DownloadConfig;
use dhow_core::engine::queues::{OnComplete, QueueSpec};
use dhow_core::engine::{AddRequest, Engine, EngineError, State};
use dhow_core::http::{Header, Url};
use dhow_core::logging;
use dhow_core::naming::TargetFs;
use dhow_core::storage::Db;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use tokio::sync::watch;

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
    },
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
                target_fs: TargetFs::host(),
            };
            let added = engine.add(req).await?;
            output::emit(output::snapshot("added", &added));
            if queue || added.state != State::Queued {
                return Ok(exit_for(added.state));
            }
            run_here(&engine, &data_dir, &added.id, &on_event, &stop).await
        }
        Command::Resume { id } => {
            let id = engine.resolve(&id)?;
            run_here(&engine, &data_dir, &id, &on_event, &stop).await
        }
        Command::Pause { id } => {
            let id = engine.resolve(&id)?;
            if let Some(pid) = lock::holder(&data_dir, &id)? {
                lock::stop_runner(&data_dir, &id, pid)?;
            } else {
                engine.pause(&id, &on_event)?;
            }
            output::emit(output::snapshot("download", &engine.get(&id)?));
            Ok(ExitCode::SUCCESS)
        }
        Command::Cancel { id, delete_partial } => {
            let id = engine.resolve(&id)?;
            if let Some(pid) = lock::holder(&data_dir, &id)? {
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
        Command::Watch => {
            let mut running = FuturesUnordered::new();
            let mut here: HashSet<String> = HashSet::new();
            let mut code = ExitCode::SUCCESS;
            loop {
                // §10.4: start what the queues have room for. Downloads
                // another process is running count as running.
                if !*stop.borrow() {
                    let next = engine.next_runnable(|id| {
                        here.contains(id) || matches!(lock::holder(&data_dir, id), Ok(Some(_)))
                    })?;
                    for snap in next {
                        let Ok(lock) = lock::acquire(&data_dir, &snap.id) else {
                            continue;
                        };
                        here.insert(snap.id.clone());
                        running.push(run_locked(&engine, lock, snap.id, &on_event, &stop));
                    }
                }
                let Some((id, result)) = running.next().await else {
                    return Ok(code);
                };
                here.remove(&id);
                match result {
                    Ok(c) if c == ExitCode::SUCCESS => {}
                    Ok(_) => code = ExitCode::FAILURE,
                    Err(e) => {
                        eprintln!("dhow: {id}: {e}");
                        code = ExitCode::FAILURE;
                    }
                }
            }
        }
        Command::Queue { command } => {
            match command {
                QueueCommand::Add {
                    name,
                    max,
                    on_complete,
                } => {
                    let spec = QueueSpec {
                        name,
                        max_concurrent: max,
                        speed_limit: None,
                        on_complete: parse_on_complete(&on_complete)?,
                    };
                    output::emit(output::queue(&engine.create_queue(&spec)?));
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
                } => {
                    let id = engine.resolve_queue(&queue)?;
                    let current = engine.queue(&id)?;
                    let spec = QueueSpec {
                        name: name.or(current.name).unwrap_or_else(|| "default".into()),
                        max_concurrent: max.unwrap_or(current.max_concurrent),
                        speed_limit: current.speed_limit,
                        on_complete: match on_complete {
                            Some(v) => parse_on_complete(&v)?,
                            None => current.on_complete,
                        },
                    };
                    output::emit(output::queue(&engine.update_queue(&id, &spec)?));
                }
                QueueCommand::Rm { queue, move_to } => {
                    let id = engine.resolve_queue(&queue)?;
                    let to = engine.resolve_queue(&move_to)?;
                    engine.delete_queue(&id, &to)?;
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
            Ok(ExitCode::SUCCESS)
        }
        Command::Reorder { id, before } => {
            let id = engine.resolve(&id)?;
            let before = before.map(|b| engine.resolve(&b)).transpose()?;
            output::emit(output::snapshot(
                "download",
                &engine.reorder(&id, before.as_deref())?,
            ));
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn parse_on_complete(v: &str) -> Res<OnComplete> {
    OnComplete::parse(v)
        .ok_or_else(|| format!("{v:?}: expected nothing, sleep, shutdown or hibernate").into())
}

/// Runs a download whose lock this process holds.
async fn run_locked(
    engine: &Engine,
    _lock: lock::RunLock,
    id: String,
    on_event: &(dyn Fn(dhow_core::engine::EngineEvent) + Send + Sync),
    stop: &watch::Receiver<bool>,
) -> (String, Res<ExitCode>) {
    let result = match engine.run(&id, on_event, stop).await {
        Ok(s) => Ok(exit_for(s.state)),
        Err(EngineError::NotFound(id)) => Err(format!("no download {id}").into()),
        Err(e) => Err(e.into()),
    };
    (id, result)
}

/// Runs a download in this process while holding its lock.
async fn run_here(
    engine: &Engine,
    data_dir: &Path,
    id: &str,
    on_event: &(dyn Fn(dhow_core::engine::EngineEvent) + Send + Sync),
    stop: &watch::Receiver<bool>,
) -> Res<ExitCode> {
    let lock = match lock::acquire(data_dir, id) {
        Ok(l) => l,
        Err(lock::Held::By(pid)) => {
            return Err(format!("download is running in process {pid}").into());
        }
        Err(lock::Held::Io(e)) => return Err(e.into()),
    };
    run_locked(engine, lock, id.to_owned(), on_event, stop)
        .await
        .1
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
