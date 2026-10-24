//! The speed ring (ENGINE-SPEC §14, ARCHITECTURE §5.1): a sample per
//! second per running download and for all of them together, kept for 10
//! minutes, then rolled into hourly rows. Sampling runs only alongside a
//! running download (§3.3: nothing ticks when idle).

use std::collections::BTreeMap;

use rusqlite::params;

use super::{Engine, EngineError};
use crate::speed::{HOUR_MS, rollup_cutoff};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeedSample {
    /// Unix ms the period began.
    pub at: i64,
    /// 1, or 3600 for an hourly row (the mean of that hour's samples).
    pub period_secs: u32,
    pub bytes_per_sec: u64,
}

/// What the global aggregate and the rollup need between samples.
#[derive(Debug, Default)]
pub(super) struct SpeedBook {
    /// Downloads sampling now.
    running: u32,
    /// Sum of the samples taken in each wall-clock second (unix ms), until
    /// that second can't get more.
    pending: BTreeMap<i64, u64>,
    /// Samples before this were rolled up.
    rolled: i64,
}

impl Engine {
    pub(super) fn sampling(&self, on: bool) -> Result<(), EngineError> {
        let mut book = self.speed();
        // Under the lock, so on/off reach the adapter in order.
        if on {
            book.running += 1;
            if book.running == 1 {
                self.platform.keep_awake(true);
            }
            return Ok(());
        }
        book.running -= 1;
        if book.running == 0 {
            self.platform.keep_awake(false);
            let all = std::mem::take(&mut book.pending);
            drop(book);
            self.write_global(all)?;
        }
        Ok(())
    }

    /// Records one download's sample taken at `now_ms`.
    pub(super) fn record_sample(
        &self,
        id: &str,
        now_ms: i64,
        bytes_per_sec: u64,
    ) -> Result<(), EngineError> {
        let second = now_ms.div_euclid(1000) * 1000;
        self.db().conn().execute(
            "INSERT INTO speed_samples (download_id, period_secs, at, bytes_per_sec)
             VALUES (?1, 1, ?2, ?3)",
            params![id, second, bytes_per_sec as i64],
        )?;
        let (done, roll) = {
            let mut book = self.speed();
            *book.pending.entry(second).or_default() += bytes_per_sec;
            // Every download samples once a second: seconds before the
            // previous one are complete.
            let open = book.pending.split_off(&(second - 1000));
            let done = std::mem::replace(&mut book.pending, open);
            let cutoff = rollup_cutoff(now_ms);
            let roll = cutoff > book.rolled;
            if roll {
                book.rolled = cutoff;
            }
            (done, roll.then_some(cutoff))
        };
        self.write_global(done)?;
        if let Some(cutoff) = roll {
            self.roll_up(cutoff)?;
        }
        Ok(())
    }

    fn write_global(&self, seconds: BTreeMap<i64, u64>) -> Result<(), EngineError> {
        if seconds.is_empty() {
            return Ok(());
        }
        let db = self.db();
        let mut stmt = db.conn().prepare_cached(
            "INSERT INTO speed_samples (download_id, period_secs, at, bytes_per_sec)
             VALUES (NULL, 1, ?1, ?2)",
        )?;
        for (at, bps) in seconds {
            stmt.execute(params![at, bps as i64])?;
        }
        Ok(())
    }

    /// Replaces the 1 s samples before `cutoff` (whole hours) by one hourly
    /// row per download and hour: the mean of its samples.
    pub(super) fn roll_up(&self, cutoff: i64) -> Result<(), EngineError> {
        let db = self.db();
        let tx = db.conn().unchecked_transaction()?;
        tx.execute(
            "INSERT INTO speed_samples (download_id, period_secs, at, bytes_per_sec)
             SELECT download_id, 3600, (at / ?2) * ?2, CAST(round(avg(bytes_per_sec)) AS INTEGER)
             FROM speed_samples WHERE period_secs = 1 AND at < ?1
             GROUP BY download_id, at / ?2",
            params![cutoff, HOUR_MS],
        )?;
        tx.execute(
            "DELETE FROM speed_samples WHERE period_secs = 1 AND at < ?1",
            [cutoff],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A download's samples, or with `None` the global aggregate's, oldest
    /// first: hourly rows, then the last minutes' 1 s samples.
    pub fn speed_history(&self, id: Option<&str>) -> Result<Vec<SpeedSample>, EngineError> {
        let db = self.db();
        let mut stmt = db.conn().prepare(
            "SELECT at, period_secs, bytes_per_sec FROM speed_samples
             WHERE download_id IS ?1 ORDER BY at, period_secs DESC",
        )?;
        let rows = stmt
            .query_map([id], |r| {
                Ok(SpeedSample {
                    at: r.get(0)?,
                    period_secs: r.get(1)?,
                    bytes_per_sec: r.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }
}
