//! Dhow engine. See `docs/ARCHITECTURE.md` and `docs/ENGINE-SPEC.md`.

pub mod complete;
pub mod download;
pub mod engine;
pub mod http;
pub mod journal;
pub mod limit;
pub mod logging;
pub mod naming;
pub mod probe;
pub mod rangeset;
pub mod schedule;
pub mod speed;
pub mod storage;
pub mod transfer;
pub mod writer;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
