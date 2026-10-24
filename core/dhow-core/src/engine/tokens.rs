//! Moved-file tracking (ARCHITECTURE §6): a token from the adapter at
//! completion (ENGINE-SPEC §8 step 8), resolved when a query finds the
//! final file gone. Never on a timer.
//!
//! Queries are synchronous, so the adapter is called on the caller's
//! thread, and only for a completed download whose file is missing; no
//! engine lock is held while it runs.

use std::path::Path;

use rusqlite::params;

use super::{Engine, EngineError, EngineEvent, Snapshot, State, path_str};
use crate::naming;
use crate::platform::{AdapterError, FileToken, PlatformAdapter};

/// A token for the file at `path`, if the platform has them.
pub(super) fn create(platform: &dyn PlatformAdapter, id: &str, path: &Path) -> Option<FileToken> {
    match platform.file_token_create(path) {
        Ok(token) => token,
        Err(AdapterError::Unsupported) => None,
        Err(AdapterError::Failed(e)) => {
            tracing::warn!(id, error = %e, "could not create a file token");
            None
        }
    }
}

impl Engine {
    /// Without a token the download is simply not tracked.
    pub(super) fn store_token(&self, id: &str, token: Option<FileToken>) {
        let stored = self.db().conn().execute(
            "UPDATE downloads SET file_token = ?2 WHERE id = ?1",
            params![id, token.map(|t| t.0)],
        );
        if let Err(e) = stored {
            tracing::warn!(id, error = %e, "could not store the file token");
        }
    }

    /// `snap`, or, if it is completed, its file is gone and its token
    /// resolves to an existing file, the download moved there: `final_path`
    /// updated, the category derived again if the name changed (§13.1),
    /// `Renamed`, then regrouped (§13.2). A file the adapter can't find, or
    /// a failing adapter, leaves it missing.
    pub(super) fn located(
        &self,
        snap: Snapshot,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<Snapshot, EngineError> {
        if snap.state != State::Completed || snap.final_path.is_file() {
            return Ok(snap);
        }
        let token: Option<Vec<u8>> = self.db().conn().query_row(
            "SELECT file_token FROM downloads WHERE id = ?1",
            [&snap.id],
            |r| r.get(0),
        )?;
        let Some(token) = token else {
            return Ok(snap);
        };
        let id = &snap.id;
        let path = match self.platform.file_token_resolve(&FileToken(token)) {
            Ok(Some(p)) if p != snap.final_path && p.is_file() => p,
            Ok(_) | Err(AdapterError::Unsupported) => return Ok(snap),
            Err(AdapterError::Failed(e)) => {
                tracing::warn!(id, error = %e, "could not resolve a file token");
                return Ok(snap);
            }
        };
        let category = match path.file_name() {
            Some(name) if Some(name) != snap.final_path.file_name() => {
                naming::categorize(&name.to_string_lossy(), None, &self.extra_extensions()?)
            }
            _ => snap.category,
        };
        // Unless another query (maybe in another process) got there first.
        let moved = self.db().conn().execute(
            "UPDATE downloads SET final_path = ?2, category = ?3
             WHERE id = ?1 AND final_path = ?4",
            params![
                id,
                path_str(&path),
                category.as_str(),
                path_str(&snap.final_path)
            ],
        )?;
        if moved == 1 {
            on_event(EngineEvent::Renamed {
                id: id.clone(),
                path,
            });
            // Grouping takes the lock itself.
            let grouped = self
                .auto_group(id)
                .and_then(|changed| self.announce_groups(&changed, on_event));
            if let Err(e) = grouped {
                tracing::warn!(id, error = %e, "could not regroup a moved download");
            }
        }
        self.stored(id)
    }
}
