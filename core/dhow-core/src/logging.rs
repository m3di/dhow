//! Structured logs: JSON lines in `<dir>/dhow.<date>.log`, rotated daily,
//! last 7 kept.
//!
//! Library code only emits `tracing` events; the embedding process (CLI,
//! Linux shell, FFI) calls [`init`] once. Writes are synchronous so the lines
//! leading up to a crash or `kill -9` are on disk.

use std::path::{Path, PathBuf};

use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;

/// `EnvFilter` syntax. Targets match by prefix, so `dhow` covers the CLI
/// binary and every `dhow_*` crate.
pub const DEFAULT_FILTER: &str = "warn,dhow=info";
const KEEP_FILES: usize = 7;

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("invalid log filter: {0}")]
    Filter(#[from] tracing_subscriber::filter::ParseError),
    #[error("cannot create log directory {}: {source}", dir.display())]
    Dir {
        dir: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot open log file: {0}")]
    Appender(#[from] tracing_appender::rolling::InitError),
    #[error("logging is already initialized")]
    AlreadyInitialized,
}

/// Installs the global subscriber writing to `dir`, and a panic hook that logs
/// the panic before the default hook runs.
pub fn init(dir: &Path, filter: &str) -> Result<(), LogError> {
    let filter = EnvFilter::try_new(filter)?;
    std::fs::create_dir_all(dir).map_err(|source| LogError::Dir {
        dir: dir.to_owned(),
        source,
    })?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("dhow")
        .filename_suffix("log")
        .max_log_files(KEEP_FILES)
        .build(dir)?;
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_env_filter(filter)
        .with_writer(appender)
        .try_init()
        .map_err(|_| LogError::AlreadyInitialized)?;

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "panic");
        default_hook(info);
    }));
    Ok(())
}

/// The platform's conventional log directory, or `None` where the shell must
/// supply one (iOS, Android: app sandbox paths only it knows).
pub fn default_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        dirs::home_dir().map(|d| d.join("Library/Logs/Dhow"))
    } else if cfg!(windows) {
        dirs::data_local_dir().map(|d| d.join("Dhow").join("logs"))
    } else if cfg!(target_os = "linux") {
        dirs::state_dir().map(|d| d.join("dhow").join("logs"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dir_is_dhow_specific() {
        let dir = default_dir().expect("desktop test hosts have a log dir");
        let s = dir.to_string_lossy().to_lowercase();
        assert!(s.contains("dhow"), "{s}");
        assert!(dir.is_absolute());
    }

    #[test]
    fn bad_filter_is_rejected_before_anything_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            init(dir.path(), "dhow_core=loud"),
            Err(LogError::Filter(_))
        ));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
