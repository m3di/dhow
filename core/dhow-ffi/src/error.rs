//! Errors across the boundary. Messages are diagnostics for logs, not for
//! display: shells word errors from the variant.

use dhow_core::engine::EngineError;
use dhow_core::platform::AdapterError as CoreAdapterError;
use dhow_core::storage::StorageError;

use crate::types::ProbeFailure;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum DhowError {
    #[error("no download, queue or group {id:?}")]
    NotFound { id: String },
    #[error("{name:?} is not a usable file name")]
    InvalidName { name: String },
    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },
    #[error("{message}")]
    InvalidQueue { message: String },
    #[error("the default queue cannot be deleted")]
    DefaultQueue,
    #[error("{message}")]
    InvalidSetting { message: String },
    #[error("probe failed: {reason:?}")]
    Probe { reason: ProbeFailure },
    /// The database was written by a newer Dhow (ARCHITECTURE §5.1).
    #[error("database schema v{found} is newer than this Dhow supports (v{supported})")]
    DatabaseTooNew { found: u32, supported: u32 },
    #[error("storage: {message}")]
    Storage { message: String },
    #[error("i/o: {message}")]
    Io { message: String },
    /// Another process (the CLI) runs this download (ARCHITECTURE §2).
    #[error("download {id} is running in another process")]
    RunningElsewhere { id: String },
    #[error("the platform adapter failed: {message}")]
    Adapter { message: String },
    /// After `shutdown`.
    #[error("the engine is shut down")]
    ShutDown,
    #[error("internal: {message}")]
    Internal { message: String },
}

impl From<EngineError> for DhowError {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::Storage(StorageError::TooNew { found, supported }) => {
                Self::DatabaseTooNew { found, supported }
            }
            EngineError::Storage(e) => Self::Storage {
                message: e.to_string(),
            },
            EngineError::Probe(e) => Self::Probe { reason: e.into() },
            EngineError::Io(e) => Self::Io {
                message: e.to_string(),
            },
            EngineError::NotFound(id) | EngineError::Ambiguous(id) => Self::NotFound { id },
            EngineError::InvalidName(name) => Self::InvalidName { name },
            EngineError::DefaultQueue => Self::DefaultQueue,
            EngineError::InvalidQueue(m) => Self::InvalidQueue { message: m.into() },
            EngineError::InvalidSetting(m) => Self::InvalidSetting { message: m.into() },
        }
    }
}

impl From<CoreAdapterError> for DhowError {
    fn from(e: CoreAdapterError) -> Self {
        Self::Adapter {
            message: e.to_string(),
        }
    }
}

/// What a [`crate::PlatformAdapter`] method returns on failure (ARCHITECTURE
/// §4, §6): `Unsupported` where the platform offers nothing, `Failed` where
/// it can't tell now.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum AdapterError {
    #[error("not supported on this platform")]
    Unsupported,
    #[error("{message}")]
    Failed { message: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for AdapterError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed { message: e.reason }
    }
}

impl From<AdapterError> for CoreAdapterError {
    fn from(e: AdapterError) -> Self {
        match e {
            AdapterError::Unsupported => Self::Unsupported,
            AdapterError::Failed { message } => Self::Failed(message),
        }
    }
}
