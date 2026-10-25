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
    /// updated (and the token, if the adapter refreshed it), the category
    /// derived again if the name changed (§13.1), `Renamed`, then regrouped
    /// (§13.2). A file the adapter reports gone loses its token; a failing
    /// adapter is asked again next time. Either way the file is missing.
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
        let old = path_str(&snap.final_path);
        let resolved = match self.platform.file_token_resolve(&FileToken(token)) {
            Ok(Some(r)) if r.path != snap.final_path && r.path.is_file() => r,
            Ok(Some(_)) | Err(AdapterError::Unsupported) => return Ok(snap),
            Ok(None) => {
                self.db().conn().execute(
                    "UPDATE downloads SET file_token = NULL WHERE id = ?1 AND final_path = ?2",
                    params![id, old],
                )?;
                return self.stored(id);
            }
            Err(AdapterError::Failed(e)) => {
                tracing::warn!(id, error = %e, "could not resolve a file token");
                return Ok(snap);
            }
        };
        let path = resolved.path;
        // A name without a known extension keeps what `Add` found, maybe
        // from the `Content-Type`.
        let category = match path.file_name() {
            Some(name) if Some(name) != snap.final_path.file_name() => {
                naming::categorize_name(&name.to_string_lossy(), &self.settings()?.extra_extensions)
                    .unwrap_or(snap.category)
            }
            _ => snap.category,
        };
        // Unless another query (maybe in another process) got there first.
        let moved = self.db().conn().execute(
            "UPDATE downloads SET final_path = ?2, category = ?3,
                 file_token = COALESCE(?5, file_token)
             WHERE id = ?1 AND final_path = ?4",
            params![
                id,
                path_str(&path),
                category.as_str(),
                old,
                resolved.token.map(|t| t.0)
            ],
        )?;
        if moved == 1 {
            on_event(EngineEvent::Renamed {
                id: id.clone(),
                path,
            });
            // Grouping takes the lock itself; off, groups stay as they are.
            let grouped = self.settings().and_then(|s| {
                if !s.auto_grouping {
                    return Ok(());
                }
                self.auto_group(id)
                    .and_then(|changed| self.announce_groups(&changed, on_event))
            });
            if let Err(e) = grouped {
                tracing::warn!(id, error = %e, "could not regroup a moved download");
            }
        }
        self.stored(id)
    }
}
