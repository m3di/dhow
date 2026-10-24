//! Queues (ENGINE-SPEC §10.4, §11): named groups of downloads with their
//! own order and concurrency. Schedules come with M2.4.

use rusqlite::{OptionalExtension, params};

use super::{Engine, EngineError, SELECT, Snapshot, snapshot};
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
    pub on_complete: OnComplete,
}

impl Default for QueueSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            max_concurrent: 3,
            speed_limit: None,
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
    pub on_complete: OnComplete,
    pub is_default: bool,
    /// Downloads in it that aren't completed or cancelled.
    pub unfinished: u32,
}

/// Downloads that want to run: not started yet, or left running by a
/// process that died (their lock is gone, see the caller's `is_running`).
const RUNNABLE: &str = "('queued', 'waiting', 'active', 'completing')";

impl Engine {
    pub fn create_queue(&self, spec: &QueueSpec) -> Result<QueueSnapshot, EngineError> {
        validate(spec)?;
        let id = uuid::Uuid::now_v7().to_string();
        self.db().conn().execute(
            "INSERT INTO queues (id, name, max_concurrent, speed_limit, on_complete)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                spec.name,
                spec.max_concurrent,
                spec.speed_limit.map(|v| v as i64),
                spec.on_complete.as_str()
            ],
        )?;
        self.queue(&id)
    }

    pub fn update_queue(&self, id: &str, spec: &QueueSpec) -> Result<QueueSnapshot, EngineError> {
        validate(spec)?;
        let n = self.db().conn().execute(
            "UPDATE queues SET name = ?2, max_concurrent = ?3, speed_limit = ?4, on_complete = ?5
             WHERE id = ?1",
            params![
                id,
                spec.name,
                spec.max_concurrent,
                spec.speed_limit.map(|v| v as i64),
                spec.on_complete.as_str()
            ],
        )?;
        if n == 0 {
            return Err(EngineError::NotFound(id.to_owned()));
        }
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

    /// What should start now (§10.4): per queue, in queue order, as many
    /// runnable downloads as its `max_concurrent` leaves room for after
    /// those already running (`is_running`: in this process or another).
    pub fn next_runnable(
        &self,
        is_running: impl Fn(&str) -> bool,
    ) -> Result<Vec<Snapshot>, EngineError> {
        let queues = self.list_queues()?;
        let mut out = Vec::new();
        for q in queues {
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
}

const QUEUE_SELECT: &str = "SELECT q.id, q.name, q.max_concurrent, q.speed_limit, q.on_complete,
    q.is_default,
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
        unfinished: r.get(6)?,
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
