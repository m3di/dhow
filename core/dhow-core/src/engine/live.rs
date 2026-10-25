//! What a shell changes on a download while it runs (ARCHITECTURE §3.1):
//! its connection count (§4.1) and whether its segment map is watched
//! (§3.3). Nothing here runs by itself; a run reads it through its
//! [`Tuning`], and the run's guard takes it out when the run ends.

use std::collections::{HashMap, HashSet};

use rusqlite::params;
use tokio::sync::watch;

use super::{DownloadId, Engine, EngineError, EngineEvent, Snapshot, State};
use crate::download::{RangeMap, Tuning};
use crate::journal::format;

#[derive(Debug, Default)]
pub(super) struct Controls {
    runs: HashMap<DownloadId, Control>,
    /// Downloads whose segment map is watched, running or not.
    watched: HashSet<DownloadId>,
}

#[derive(Debug)]
struct Control {
    connections: watch::Sender<Option<u8>>,
    ranges: watch::Sender<bool>,
}

/// See [`Engine::take_control`].
pub(super) struct ControlGuard<'a> {
    engine: &'a Engine,
    id: &'a str,
    ranges: watch::Sender<bool>,
}

impl Drop for ControlGuard<'_> {
    fn drop(&mut self) {
        let mut c = self.engine.controls();
        // Only its own: a later run of the same id may have replaced it.
        if c.runs
            .get(self.id)
            .is_some_and(|o| o.ranges.same_channel(&self.ranges))
        {
            c.runs.remove(self.id);
        }
    }
}

impl Engine {
    /// A download's connection override (§4.1; `None`: the default). A
    /// running download follows at once: up through ramp-up (§4.4), down by
    /// stopping its newest connections.
    pub fn set_connections(&self, id: &str, n: Option<u8>) -> Result<Snapshot, EngineError> {
        let n = n.map(|n| n.max(1));
        {
            // Under the controls' lock: a run starting meanwhile reads it
            // after.
            let c = self.controls();
            let changed = self.db().conn().execute(
                "UPDATE downloads SET connections = ?2 WHERE id = ?1",
                params![id, n],
            )?;
            if changed == 0 {
                return Err(EngineError::NotFound(id.to_owned()));
            }
            if let Some(run) = c.runs.get(id) {
                run.connections.send_replace(n);
            }
        }
        self.stored(id)
    }

    /// Turns `RangeMap` events for a download on or off (ARCHITECTURE
    /// §3.3). On: one at once, from what is committed; while it runs, then
    /// at most one a second until off or the run ends, and one more from
    /// the sidecar when it ends. Nothing runs for it otherwise.
    pub fn watch_ranges(
        &self,
        id: &str,
        on: bool,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<(), EngineError> {
        let snap = self.stored(id)?;
        {
            let mut c = self.controls();
            if on {
                c.watched.insert(id.to_owned());
            } else {
                c.watched.remove(id);
            }
            if let Some(run) = c.runs.get(id) {
                run.ranges.send_replace(on);
            }
        }
        if on {
            on_event(EngineEvent::RangeMap {
                id: id.to_owned(),
                map: stored_map(&snap),
            });
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn running_controls(&self) -> usize {
        self.controls().runs.len()
    }

    pub(super) fn watched(&self, id: &str) -> bool {
        self.controls().watched.contains(id)
    }

    pub(super) fn forget_watch(&self, id: &str) {
        self.controls().watched.remove(id);
    }

    /// A run's [`Tuning`], registered until the guard drops (also when the
    /// `run` future is dropped).
    pub(super) fn take_control<'a>(
        &'a self,
        id: &'a str,
    ) -> Result<(Tuning, ControlGuard<'a>), EngineError> {
        let mut c = self.controls();
        let (connections, connections_rx) = watch::channel(self.connections(id)?);
        let (ranges, ranges_rx) = watch::channel(c.watched.contains(id));
        let guard = ControlGuard {
            engine: self,
            id,
            ranges: ranges.clone(),
        };
        c.runs.insert(
            id.to_owned(),
            Control {
                connections,
                ranges,
            },
        );
        let tuning = Tuning {
            connections: connections_rx,
            ranges: ranges_rx,
        };
        Ok((tuning, guard))
    }
}

/// The segment map of a download that isn't running: its sidecar's
/// committed ranges, the whole file once completed.
#[allow(clippy::single_range_in_vec_init)]
pub(super) fn stored_map(snap: &Snapshot) -> RangeMap {
    let total = snap.total_size;
    if snap.state == State::Completed {
        let len = total.unwrap_or(snap.downloaded);
        return RangeMap {
            total: Some(len),
            done: vec![0..len],
            ..RangeMap::default()
        };
    }
    let committed = std::fs::read(snap.sidecar_path())
        .ok()
        .and_then(|b| format::replay(&b).ok())
        .map(|r| r.committed)
        .unwrap_or_default();
    RangeMap {
        total,
        done: committed.ranges().to_vec(),
        connections: Vec::new(),
        pending: total.map(|t| committed.gaps(t)).unwrap_or_default(),
    }
}
