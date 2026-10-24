//! Queues (ENGINE-SPEC §10.4, §11): named groups of downloads with their
//! own order, concurrency and schedule.
//!
//! Functions taking `now` are the policy; the ones without use the system
//! clock and time zone.

use jiff::{Timestamp, Zoned};
use rusqlite::{OptionalExtension, params};

use super::{Engine, EngineError, SELECT, Snapshot, snapshot};
use crate::schedule::{Action, Schedule};
use crate::storage::DEFAULT_QUEUE_ID;

pub type QueueId = String;

/// What to do when a queue runs dry. The shell decides what's possible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnComplete {
    #[default]
    Nothing,
    Sleep,
    Shutdown,
    Hibernate,
}

impl OnComplete {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nothing => "nothing",
            Self::Sleep => "sleep",
            Self::Shutdown => "shutdown",
            Self::Hibernate => "hibernate",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "nothing" => Self::Nothing,
            "sleep" => Self::Sleep,
            "shutdown" => Self::Shutdown,
            "hibernate" => Self::Hibernate,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSpec {
    pub name: String,
    pub max_concurrent: u32,
    /// Bytes/s for the whole queue (§12, M2.5).
    pub speed_limit: Option<u64>,
    pub schedule: Option<Schedule>,
    pub on_complete: OnComplete,
}

/// A `StartQueue`/`StopQueue` override of the schedule (§11).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunState {
    /// Follow the schedule (always on without one).
    #[default]
    Auto,
    Started,
    Stopped,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Started => "started",
            Self::Stopped => "stopped",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "started" => Self::Started,
            "stopped" => Self::Stopped,
            _ => Self::Auto,
        }
    }
}

impl Default for QueueSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            max_concurrent: 3,
            speed_limit: None,
            schedule: None,
            on_complete: OnComplete::Nothing,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub id: QueueId,
    /// `None` for the default queue until renamed: the shell names it.
    pub name: Option<String>,
    pub max_concurrent: u32,
    pub speed_limit: Option<u64>,
    pub schedule: Option<Schedule>,
    pub on_complete: OnComplete,
    pub is_default: bool,
    pub run_state: RunState,
    /// Unix ms the override lasts until; `None`: until changed.
    pub run_state_until: Option<i64>,
    /// Downloads in it that aren't completed or cancelled.
    pub unfinished: u32,
}

impl QueueSnapshot {
    /// The override still in force at `now`, if any.
    fn run_override(&self, now: Timestamp) -> Option<RunState> {
        let live = self
            .run_state_until
            .is_none_or(|u| now.as_millisecond() < u);
        (self.run_state != RunState::Auto && live).then_some(self.run_state)
    }

    /// May its downloads start now? (§11)
    pub fn active_at(&self, now: &Zoned) -> bool {
        match self.run_override(now.timestamp()) {
            Some(RunState::Started) => true,
            Some(RunState::Stopped) => false,
            _ => self.schedule.as_ref().is_none_or(|s| s.inside(now)),
        }
    }

    /// Must its running downloads stop now? Stopped by hand, or outside a
    /// `StartAndStopOutside` window. (`Start` lets them run past the end.)
    pub fn must_stop_at(&self, now: &Zoned) -> bool {
        if self.active_at(now) {
            return false;
        }
        self.run_override(now.timestamp()) == Some(RunState::Stopped)
            || self
                .schedule
                .as_ref()
                .is_some_and(|s| s.action == Action::StartAndStopOutside)
    }

    /// When this queue's state can next change: a schedule edge or the end
    /// of an override.
    pub fn next_edge_at(&self, now: &Zoned) -> Option<Timestamp> {
        let t = now.timestamp();
        let schedule = self.schedule.as_ref().and_then(|s| s.next_edge(now));
        let until = self
            .run_override(t)
            .and(self.run_state_until)
            .and_then(|ms| Timestamp::from_millisecond(ms).ok());
        [schedule, until]
            .into_iter()
            .flatten()
            .filter(|&e| e > t)
            .min()
    }
}

/// Downloads that want to run: not started yet, or left running by a
/// process that died (their lock is gone, see the caller's `is_running`).
const RUNNABLE: &str = "('queued', 'waiting', 'active', 'completing')";

impl Engine {
    pub fn create_queue(&self, spec: &QueueSpec) -> Result<QueueSnapshot, EngineError> {
        validate(spec)?;
        let id = uuid::Uuid::now_v7().to_string();
        self.db().conn().execute(
            "INSERT INTO queues (id, name, max_concurrent, speed_limit, schedule, on_complete)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                spec.name,
                spec.max_concurrent,
                spec.speed_limit.map(|v| v as i64),
                schedule_json(spec),
                spec.on_complete.as_str()
            ],
        )?;
        self.queue(&id)
    }

    pub fn update_queue(&self, id: &str, spec: &QueueSpec) -> Result<QueueSnapshot, EngineError> {
        validate(spec)?;
        if self.queue(id)?.is_default && spec.schedule.is_some() {
            return Err(EngineError::InvalidQueue(
                "the default queue has no schedule",
            ));
        }
        self.db().conn().execute(
            "UPDATE queues SET name = ?2, max_concurrent = ?3, speed_limit = ?4, schedule = ?5,
                 on_complete = ?6
             WHERE id = ?1",
            params![
                id,
                spec.name,
                spec.max_concurrent,
                spec.speed_limit.map(|v| v as i64),
                schedule_json(spec),
                spec.on_complete.as_str()
            ],
        )?;
        self.queue(id)
    }

    /// Deletes a queue, appending its downloads to `move_items_to` in their
    /// current order. The default queue can't be deleted (§11).
    pub fn delete_queue(&self, id: &str, move_items_to: &str) -> Result<(), EngineError> {
        let victim = self.queue(id)?;
        if victim.is_default {
            return Err(EngineError::DefaultQueue);
        }
        self.queue(move_items_to)?;
        if id == move_items_to {
            return Err(EngineError::InvalidQueue(
                "cannot move a queue's downloads into itself",
            ));
        }
        let db = self.db();
        let tx = db.conn().unchecked_transaction()?;
        let base: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM downloads WHERE queue_id = ?1",
            [move_items_to],
            |r| r.get(0),
        )?;
        let ids = ordered_ids(&tx, id)?;
        for (i, dl) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE downloads SET queue_id = ?2, position = ?3 WHERE id = ?1",
                params![dl, move_items_to, base + i as i64],
            )?;
        }
        tx.execute("DELETE FROM queues WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// The default queue first, then by name.
    pub fn list_queues(&self) -> Result<Vec<QueueSnapshot>, EngineError> {
        let db = self.db();
        let mut stmt = db
            .conn()
            .prepare(&format!("{QUEUE_SELECT} ORDER BY is_default DESC, name"))?;
        let rows = stmt
            .query_map([], queue_snapshot)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn queue(&self, id: &str) -> Result<QueueSnapshot, EngineError> {
        let db = self.db();
        let q = db
            .conn()
            .query_row(
                &format!("{QUEUE_SELECT} WHERE id = ?1"),
                [id],
                queue_snapshot,
            )
            .optional()?;
        q.ok_or_else(|| EngineError::NotFound(id.to_owned()))
    }

    /// `default`, an exact name, or a unique id prefix.
    pub fn resolve_queue(&self, name_or_id: &str) -> Result<QueueId, EngineError> {
        if name_or_id == "default" {
            return Ok(DEFAULT_QUEUE_ID.to_owned());
        }
        let db = self.db();
        let by_name: Option<String> = db
            .conn()
            .query_row("SELECT id FROM queues WHERE name = ?1", [name_or_id], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(id) = by_name {
            return Ok(id);
        }
        let mut stmt = db
            .conn()
            .prepare("SELECT id FROM queues WHERE substr(id, 1, length(?1)) = ?1 LIMIT 2")?;
        let ids: Vec<String> = stmt
            .query_map([name_or_id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        match ids.as_slice() {
            [id] => Ok(id.clone()),
            [] => Err(EngineError::NotFound(name_or_id.to_owned())),
            _ => Err(EngineError::Ambiguous(name_or_id.to_owned())),
        }
    }

    /// Moves a download to the end of another queue.
    pub fn move_to_queue(&self, id: &str, queue: &str) -> Result<Snapshot, EngineError> {
        self.get(id)?;
        self.queue(queue)?;
        let db = self.db();
        let position: i64 = db.conn().query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM downloads WHERE queue_id = ?1",
            [queue],
            |r| r.get(0),
        )?;
        db.conn().execute(
            "UPDATE downloads SET queue_id = ?2, position = ?3 WHERE id = ?1",
            params![id, queue, position],
        )?;
        drop(db);
        self.get(id)
    }

    /// Moves a download before `before` (same queue), or to the end of its
    /// queue. Positions are renumbered 0.. in queue order.
    pub fn reorder(&self, id: &str, before: Option<&str>) -> Result<Snapshot, EngineError> {
        let me = self.get(id)?;
        if let Some(b) = before {
            if b == id {
                return Ok(me);
            }
            if self.get(b)?.queue_id != me.queue_id {
                return Err(EngineError::InvalidQueue("can only reorder within a queue"));
            }
        }
        let db = self.db();
        let tx = db.conn().unchecked_transaction()?;
        let mut ids: Vec<String> = ordered_ids(&tx, &me.queue_id)?
            .into_iter()
            .filter(|x| x != id)
            .collect();
        let at = before
            .and_then(|b| ids.iter().position(|x| x == b))
            .unwrap_or(ids.len());
        ids.insert(at, id.to_owned());
        for (i, dl) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE downloads SET position = ?2 WHERE id = ?1",
                params![dl, i as i64],
            )?;
        }
        tx.commit()?;
        drop(db);
        self.get(id)
    }

    /// `StartQueue` (§11): run now, overriding the schedule until its next
    /// edge (indefinitely for a queue without one).
    pub fn start_queue(&self, id: &str) -> Result<QueueSnapshot, EngineError> {
        self.set_run_state(id, RunState::Started, &Zoned::now())
    }

    /// `StopQueue` (§11): don't start anything, and stop what runs, until
    /// the next schedule edge (indefinitely without a schedule).
    pub fn stop_queue(&self, id: &str) -> Result<QueueSnapshot, EngineError> {
        self.set_run_state(id, RunState::Stopped, &Zoned::now())
    }

    /// Back to following the schedule.
    pub fn resume_schedule(&self, id: &str) -> Result<QueueSnapshot, EngineError> {
        self.set_run_state(id, RunState::Auto, &Zoned::now())
    }

    pub fn set_run_state(
        &self,
        id: &str,
        state: RunState,
        now: &Zoned,
    ) -> Result<QueueSnapshot, EngineError> {
        let q = self.queue(id)?;
        let until = match state {
            RunState::Auto => None,
            _ => q
                .schedule
                .as_ref()
                .and_then(|s| s.next_edge(now))
                .map(|t| t.as_millisecond()),
        };
        self.db().conn().execute(
            "UPDATE queues SET run_state = ?2, run_state_until = ?3 WHERE id = ?1",
            params![id, state.as_str(), until],
        )?;
        self.queue(id)
    }

    /// What should start now (§10.4, §11), by the system clock.
    pub fn next_runnable(
        &self,
        is_running: impl Fn(&str) -> bool,
    ) -> Result<Vec<Snapshot>, EngineError> {
        self.next_runnable_at(&Zoned::now(), is_running)
    }

    /// Per active queue, in queue order, as many runnable downloads as its
    /// `max_concurrent` leaves room for after those already running
    /// (`is_running`: in this process or another).
    pub fn next_runnable_at(
        &self,
        now: &Zoned,
        is_running: impl Fn(&str) -> bool,
    ) -> Result<Vec<Snapshot>, EngineError> {
        let queues = self.list_queues()?;
        let mut out = Vec::new();
        for q in queues.into_iter().filter(|q| q.active_at(now)) {
            let rows: Vec<Snapshot> = {
                let db = self.db();
                let mut stmt = db.conn().prepare(&format!(
                    "{SELECT} WHERE queue_id = ?1 AND state IN {RUNNABLE} ORDER BY position, created_at"
                ))?;
                stmt.query_map([&q.id], snapshot)?
                    .collect::<Result<_, _>>()?
            };
            let (running, waiting): (Vec<_>, Vec<_>) =
                rows.into_iter().partition(|s| is_running(&s.id));
            let room = (q.max_concurrent as usize).saturating_sub(running.len());
            out.extend(waiting.into_iter().take(room));
        }
        Ok(out)
    }

    /// Of the downloads running (`running`), those whose queue must stop
    /// them now (§11).
    pub fn to_stop_at<'a>(
        &self,
        now: &Zoned,
        running: impl IntoIterator<Item = &'a str>,
    ) -> Result<Vec<String>, EngineError> {
        let stopping: Vec<String> = self
            .list_queues()?
            .into_iter()
            .filter(|q| q.must_stop_at(now))
            .map(|q| q.id)
            .collect();
        if stopping.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for id in running {
            if stopping.contains(&self.get(id)?.queue_id) {
                out.push(id.to_owned());
            }
        }
        Ok(out)
    }

    pub fn to_stop<'a>(
        &self,
        running: impl IntoIterator<Item = &'a str>,
    ) -> Result<Vec<String>, EngineError> {
        self.to_stop_at(&Zoned::now(), running)
    }

    /// The earliest moment any queue's state can change, or `None`: the
    /// single wake-up a runner sleeps until (ARCHITECTURE §7). No
    /// schedules and no timed overrides, no wake-up.
    pub fn next_edge_at(&self, now: &Zoned) -> Result<Option<Timestamp>, EngineError> {
        Ok(self
            .list_queues()?
            .iter()
            .filter_map(|q| q.next_edge_at(now))
            .min())
    }

    pub fn next_edge(&self) -> Result<Option<Timestamp>, EngineError> {
        self.next_edge_at(&Zoned::now())
    }

    /// Is any download waiting to run (in any queue, active or not)?
    pub fn has_waiting_work(&self) -> Result<bool, EngineError> {
        let n: i64 = self.db().conn().query_row(
            &format!("SELECT count(*) FROM downloads WHERE state IN {RUNNABLE}"),
            [],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
}

fn schedule_json(spec: &QueueSpec) -> Option<String> {
    spec.schedule
        .as_ref()
        .map(|s| serde_json::to_string(s).expect("schedules serialize"))
}

const QUEUE_SELECT: &str = "SELECT q.id, q.name, q.max_concurrent, q.speed_limit, q.on_complete,
    q.is_default, q.schedule, q.run_state, q.run_state_until,
    (SELECT count(*) FROM downloads d WHERE d.queue_id = q.id
        AND d.state NOT IN ('completed', 'cancelled'))
    FROM queues q";

fn queue_snapshot(r: &rusqlite::Row) -> rusqlite::Result<QueueSnapshot> {
    Ok(QueueSnapshot {
        id: r.get(0)?,
        name: r.get(1)?,
        max_concurrent: r.get(2)?,
        speed_limit: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
        on_complete: OnComplete::parse(&r.get::<_, String>(4)?).unwrap_or_default(),
        is_default: r.get(5)?,
        schedule: r
            .get::<_, Option<String>>(6)?
            .and_then(|j| match serde_json::from_str(&j) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(error = %e, "ignoring unreadable queue schedule");
                    None
                }
            }),
        run_state: RunState::parse(&r.get::<_, String>(7)?),
        run_state_until: r.get(8)?,
        unfinished: r.get(9)?,
    })
}

fn ordered_ids(conn: &rusqlite::Connection, queue: &str) -> Result<Vec<String>, EngineError> {
    let mut stmt =
        conn.prepare("SELECT id FROM downloads WHERE queue_id = ?1 ORDER BY position, created_at")?;
    let ids = stmt
        .query_map([queue], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

fn validate(spec: &QueueSpec) -> Result<(), EngineError> {
    if spec.schedule.as_ref().is_some_and(|s| s.windows.is_empty()) {
        return Err(EngineError::InvalidQueue(
            "a schedule needs at least one window",
        ));
    }
    if spec.name.trim().is_empty() {
        return Err(EngineError::InvalidQueue("a queue needs a name"));
    }
    if spec.max_concurrent == 0 {
        return Err(EngineError::InvalidQueue(
            "max_concurrent must be at least 1",
        ));
    }
    Ok(())
}
