//! The SQLite index (`ARCHITECTURE §5.1`). A cache of what the sidecars say
//! plus queues, groups and settings; never the integrity authority.
//!
//! Schema version lives in `PRAGMA user_version`. Migration `n` (1-based)
//! takes the schema from `n - 1` to `n` and runs in one transaction together
//! with the version bump, so a failed migration leaves the database as it was.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// Append-only. Never edit a migration that has shipped; add a new one.
const MIGRATIONS: &[&str] = &[include_str!("migrations/0001_initial.sql")];

pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;
pub const DEFAULT_QUEUE_ID: &str = "00000000-0000-0000-0000-000000000000";

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("cannot create database directory {}: {source}", dir.display())]
    Dir {
        dir: PathBuf,
        source: std::io::Error,
    },
    #[error("database schema v{found} is newer than this Dhow supports (v{supported})")]
    TooNew { found: u32, supported: u32 },
    #[error("migration to v{version} failed: {source}")]
    Migration {
        version: u32,
        source: rusqlite::Error,
    },
    #[error("database setting {pragma} could not be applied")]
    Pragma { pragma: &'static str },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Opens or creates the database at `path` and migrates it to
    /// [`SCHEMA_VERSION`].
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| StorageError::Dir {
                dir: dir.to_owned(),
                source,
            })?;
        }
        let mut conn = Connection::open(path)?;
        configure(&conn)?;
        migrate(&mut conn, MIGRATIONS)?;
        Ok(Self { conn })
    }

    pub fn schema_version(&self) -> Result<u32, StorageError> {
        user_version(&self.conn)
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The platform's conventional data directory for `dhow.sqlite`, or
    /// `None` where the shell must supply one (iOS, Android).
    pub fn default_dir() -> Option<PathBuf> {
        if cfg!(target_os = "macos") {
            dirs::home_dir().map(|d| d.join("Library/Application Support/Dhow"))
        } else if cfg!(windows) {
            dirs::data_local_dir().map(|d| d.join("Dhow"))
        } else if cfg!(target_os = "linux") {
            dirs::data_dir().map(|d| d.join("dhow"))
        } else {
            None
        }
    }
}

fn configure(conn: &Connection) -> Result<(), StorageError> {
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StorageError::Pragma {
            pragma: "journal_mode",
        });
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    let fk: bool = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
    if !fk {
        return Err(StorageError::Pragma {
            pragma: "foreign_keys",
        });
    }
    Ok(())
}

fn user_version(conn: &Connection) -> Result<u32, StorageError> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<(), StorageError> {
    let supported = migrations.len() as u32;
    let found = user_version(conn)?;
    if found > supported {
        return Err(StorageError::TooNew { found, supported });
    }
    for (version, sql) in (found + 1..).zip(&migrations[found as usize..]) {
        let failed = |source| StorageError::Migration { version, source };
        let tx = conn.transaction().map_err(failed)?;
        tx.execute_batch(sql).map_err(failed)?;
        tx.pragma_update(None, "user_version", version)
            .map_err(failed)?;
        tx.commit().map_err(failed)?;
        tracing::info!(version, "database migrated");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
