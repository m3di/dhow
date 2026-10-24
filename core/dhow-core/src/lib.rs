//! Dhow engine. See `docs/ARCHITECTURE.md` and `docs/ENGINE-SPEC.md`.

pub mod http;
pub mod journal;
pub mod logging;
pub mod naming;
pub mod probe;
pub mod rangeset;
pub mod storage;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
