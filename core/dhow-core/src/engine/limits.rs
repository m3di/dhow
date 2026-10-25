//! Speed limits (ENGINE-SPEC §12, §10.6): where they are stored, and the
//! buckets of the downloads running in this process. A limit changed by
//! another process applies here on [`Engine::refresh_limits`]. The global
//! limit and headroom are settings ([`super::settings`]).

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::{OptionalExtension, params};

use super::queues::QueueId;
use super::{DownloadId, Engine, EngineError, Snapshot};
use crate::limit::Bucket;

/// Buckets of what runs here: one per queue (shared by its downloads) and
/// one per running download.
#[derive(Debug, Default)]
pub(super) struct Buckets {
    queues: HashMap<QueueId, Arc<Bucket>>,
    downloads: HashMap<DownloadId, Arc<Bucket>>,
}

/// See [`Engine::take_buckets`].
pub(super) struct OwnBucket<'a> {
    engine: &'a Engine,
    id: &'a str,
    bucket: Arc<Bucket>,
}

impl Drop for OwnBucket<'_> {
    fn drop(&mut self) {
        let mut b = self.engine.buckets();
        // Only its own: a later run of the same id may have replaced it.
        if b.downloads
            .get(self.id)
            .is_some_and(|o| Arc::ptr_eq(o, &self.bucket))
        {
            b.downloads.remove(self.id);
        }
    }
}

impl Engine {
    /// A download's own limit; `Some(0)` pauses it by limit (§12).
    pub fn set_speed_limit(&self, id: &str, rate: Option<u64>) -> Result<Snapshot, EngineError> {
        let changed = self.db().conn().execute(
            "UPDATE downloads SET speed_limit = ?2 WHERE id = ?1",
            params![id, rate.map(|r| r as i64)],
        )?;
        if changed == 0 {
            return Err(EngineError::NotFound(id.to_owned()));
        }
        self.refresh_limits()?;
        self.stored(id)
    }

    /// Applies the stored limits to what runs in this process: after a
    /// change here or, told so, in another process.
    pub fn refresh_limits(&self) -> Result<(), EngineError> {
        let settings = self.settings()?;
        self.shared.global.set_user(settings.global_speed_limit);
        self.shared.global.set_headroom(settings.headroom_percent);
        let (queues, downloads): (Vec<_>, Vec<_>) = {
            let b = self.buckets();
            (
                b.queues
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                b.downloads
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            )
        };
        for (id, bucket) in queues {
            // A deleted queue's bucket has no downloads left to limit.
            if let Ok(q) = self.queue(&id) {
                bucket.set_rate(q.speed_limit);
            }
        }
        for (id, bucket) in downloads {
            bucket.set_rate(self.download_limit(&id)?);
        }
        Ok(())
    }

    /// The queue's and the download's buckets for a run of `id`. The
    /// download's stays registered until the guard drops, also when the
    /// `run` future is dropped.
    pub(super) fn take_buckets<'a>(
        &'a self,
        id: &'a str,
        queue_id: &str,
    ) -> Result<(Vec<Arc<Bucket>>, OwnBucket<'a>), EngineError> {
        let queue_rate = self.queue(queue_id)?.speed_limit;
        let own = self.shared.bucket(self.download_limit(id)?);
        let mut b = self.buckets();
        let queue = b
            .queues
            .entry(queue_id.to_owned())
            .or_insert_with(|| self.shared.bucket(queue_rate))
            .clone();
        queue.set_rate(queue_rate);
        b.downloads.insert(id.to_owned(), own.clone());
        let guard = OwnBucket {
            engine: self,
            id,
            bucket: own.clone(),
        };
        Ok((vec![queue, own], guard))
    }

    #[cfg(test)]
    pub(super) fn own_buckets(&self) -> usize {
        self.buckets().downloads.len()
    }

    fn download_limit(&self, id: &str) -> Result<Option<u64>, EngineError> {
        let rate: Option<i64> = self
            .db()
            .conn()
            .query_row(
                "SELECT speed_limit FROM downloads WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(rate.map(|r| r as u64))
    }
}
