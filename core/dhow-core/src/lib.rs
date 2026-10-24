//! Dhow engine. See `docs/ARCHITECTURE.md` and `docs/ENGINE-SPEC.md`.

pub mod logging;
pub mod storage;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
