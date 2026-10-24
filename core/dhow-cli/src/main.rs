mod lock;
mod output;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use dhow_core::download::DownloadConfig;
use dhow_core::engine::{AddRequest, Engine, EngineError, State};
use dhow_core::http::{Header, Url};
use dhow_core::logging;
use dhow_core::naming::TargetFs;
use dhow_core::storage::Db;
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
    /// Run every queued download, one after another, then exit
    Watch,
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
        } => {
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
            let mut code = ExitCode::SUCCESS;
            loop {
                let next = engine.list()?.into_iter().find(|s| {
                    s.state == State::Queued && matches!(lock::holder(&data_dir, &s.id), Ok(None))
                });
                let Some(next) = next else { return Ok(code) };
                if *stop.borrow() {
                    return Ok(code);
                }
                if run_here(&engine, &data_dir, &next.id, &on_event, &stop).await?
                    != ExitCode::SUCCESS
                {
                    code = ExitCode::FAILURE;
                }
            }
        }
    }
}

/// Runs a download in this process while holding its lock.
async fn run_here(
    engine: &Engine,
    data_dir: &Path,
    id: &str,
    on_event: &(dyn Fn(dhow_core::engine::EngineEvent) + Send + Sync),
    stop: &watch::Receiver<bool>,
) -> Res<ExitCode> {
    let _lock = match lock::acquire(data_dir, id) {
        Ok(l) => l,
        Err(lock::Held::By(pid)) => {
            return Err(format!("download is running in process {pid}").into());
        }
        Err(lock::Held::Io(e)) => return Err(e.into()),
    };
    let snap = match engine.run(id, on_event, stop).await {
        Ok(s) => s,
        Err(EngineError::NotFound(id)) => return Err(format!("no download {id}").into()),
        Err(e) => return Err(e.into()),
    };
    Ok(exit_for(snap.state))
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
