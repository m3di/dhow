//! Speed limits (ENGINE-SPEC §12, §10.6): where they are stored, and the
//! buckets of the downloads running in this process. A limit changed by
//! another process applies here on [`Engine::refresh_limits`].

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::{OptionalExtension, params};

use super::queues::QueueId;
use super::{DownloadId, Engine, EngineError, Snapshot};
use crate::limit::Bucket;

/// The process-wide limits (`settings` table).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimitSettings {
    /// Bytes/s for everything together; `None`: unlimited.
    pub global: Option<u64>,
    /// Share of the link to leave free (§10.6), 0–99.
    pub headroom_percent: u8,
}

const GLOBAL_KEY: &str = "global_speed_limit";
const HEADROOM_KEY: &str = "headroom_percent";

/// Buckets of what runs here: one per queue (shared by its downloads) and
/// one per running download.
#[derive(Debug, Default)]
pub(super) struct Buckets {
    queues: HashMap<QueueId, Arc<Bucket>>,
    downloads: HashMap<DownloadId, Arc<Bucket>>,
}

impl Engine {
    pub fn limit_settings(&self) -> Result<LimitSettings, EngineError> {
        let global: Option<u64> = self.setting(GLOBAL_KEY)?.flatten();
        let headroom: Option<u8> = self.setting(HEADROOM_KEY)?.flatten();
        Ok(LimitSettings {
            global,
            headroom_percent: headroom.unwrap_or(0),
        })
    }

    pub fn set_global_limit(&self, rate: Option<u64>) -> Result<LimitSettings, EngineError> {
        self.put_setting(GLOBAL_KEY, &rate)?;
        self.refresh_limits()?;
        self.limit_settings()
    }

    pub fn set_headroom(&self, percent: u8) -> Result<LimitSettings, EngineError> {
        if percent > 99 {
            return Err(EngineError::InvalidSetting("headroom is 0–99 %"));
        }
        self.put_setting(HEADROOM_KEY, &percent)?;
        self.refresh_limits()?;
        self.limit_settings()
    }

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
        let settings = self.limit_settings()?;
        self.shared.global.set_user(settings.global);
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

    /// The queue's and the download's buckets for a run of `id`.
    pub(super) fn take_buckets(
        &self,
        id: &str,
        queue_id: &str,
    ) -> Result<Vec<Arc<Bucket>>, EngineError> {
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
        Ok(vec![queue, own])
    }

    pub(super) fn drop_buckets(&self, id: &str) {
        self.buckets().downloads.remove(id);
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

    pub(super) fn setting<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, EngineError> {
        let json: Option<String> = self
            .db()
            .conn()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(json.and_then(|j| match serde_json::from_str(&j) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(key, error = %e, "ignoring unreadable setting");
                None
            }
        }))
    }

    pub(super) fn put_setting(
        &self,
        key: &str,
        value: &impl serde::Serialize,
    ) -> Result<(), EngineError> {
        let json = serde_json::to_string(value).expect("settings serialize");
        self.db().conn().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, json],
        )?;
        Ok(())
    }
}
