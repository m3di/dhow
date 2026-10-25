//! UniFFI bindings for dhow-core (ARCHITECTURE §9): an [`Engine`] object
//! that owns its runtime and runs downloads in process, plain records and
//! enums mirroring the core's types, events to a foreign [`EventListener`]
//! and the platform adapter as a foreign [`PlatformAdapter`]. Swift and
//! Kotlin sources are generated from the built library by the
//! `uniffi-bindgen` binary (see `build-swift.sh`).

mod adapter;
mod engine;
mod error;
mod events;
mod types;

pub use adapter::{AdapterNetworkInfo, PlatformAdapter, ResolvedFile};
pub use engine::Engine;
pub use error::{AdapterError, DhowError};
pub use events::EventListener;
pub use types::*;

uniffi::setup_scaffolding!();

/// Installs the process's log writer (ARCHITECTURE §5.3): JSON lines in
/// `dir`, rotated daily. Once per process; `filter` is `EnvFilter` syntax,
/// `None` the default.
#[uniffi::export]
pub fn init_logging(dir: String, filter: Option<String>) -> Result<(), DhowError> {
    let filter = filter.unwrap_or_else(|| dhow_core::logging::DEFAULT_FILTER.to_owned());
    dhow_core::logging::init(dir.as_ref(), &filter).map_err(|e| DhowError::Io {
        message: e.to_string(),
    })
}

/// The platform's log directory, or `None` where the shell must give one.
#[uniffi::export]
pub fn default_log_dir() -> Option<String> {
    dhow_core::logging::default_dir().map(|d| d.to_string_lossy().into_owned())
}

/// The platform's data directory (as the CLI uses), or `None` where the
/// shell must give one (iOS, sandboxed apps).
#[uniffi::export]
pub fn default_data_dir() -> Option<String> {
    dhow_core::storage::Db::default_dir().map(|d| d.to_string_lossy().into_owned())
}

#[uniffi::export]
pub fn core_version() -> String {
    dhow_core::VERSION.to_owned()
}
