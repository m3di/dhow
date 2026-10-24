//! Groups (ENGINE-SPEC §13.2): formed by name on `Add`; `SetGroup` and
//! `RenameGroup` make them manual, and auto-grouping leaves manual groups
//! and downloads the user placed alone.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{
    DownloadId, Engine, EngineError, EngineEvent, SELECT, Snapshot, State, now_ms, snapshot,
};
use crate::naming;
use crate::speed::Ema;

pub type GroupId = String;

/// A member whose newest 1 s sample is older than this isn't moving.
const STALE_MS: i64 = 3000;
/// A member's current speed is the EMA of its samples this recent.
const RECENT_MS: i64 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupSnapshot {
    pub id: GroupId,
    pub name: String,
    /// `false` once the user changed it.
    pub auto: bool,
    /// Oldest first.
    pub members: Vec<DownloadId>,
    /// Of the members not cancelled.
    pub downloaded: u64,
    /// `None` if any such member's size is unknown.
    pub total: Option<u64>,
    /// Sum of the running members' speeds (§14).
    pub speed_bps: u64,
    /// For what the unfinished members have left, at `speed_bps`.
    pub eta_secs: Option<u64>,
}

struct Member {
    id: DownloadId,
    dir: PathBuf,
    name: String,
    group: Option<GroupId>,
    auto: bool,
    pinned: bool,
}

impl Engine {
    /// Oldest first.
    pub fn list_groups(&self) -> Result<Vec<GroupSnapshot>, EngineError> {
        let ids: Vec<GroupId> = {
            let db = self.db();
            let mut stmt = db.conn().prepare("SELECT id FROM groups ORDER BY id")?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        ids.iter().map(|id| self.group(id)).collect()
    }

    /// With aggregates computed now from the members.
    pub fn group(&self, id: &str) -> Result<GroupSnapshot, EngineError> {
        let (head, members) = {
            let db = self.db();
            let head: Option<(String, bool)> = db
                .conn()
                .query_row("SELECT name, auto FROM groups WHERE id = ?1", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?;
            let mut stmt = db.conn().prepare(&format!(
                "{SELECT} WHERE group_id = ?1 ORDER BY created_at, id"
            ))?;
            let members: Vec<Snapshot> =
                stmt.query_map([id], snapshot)?.collect::<Result<_, _>>()?;
            (head, members)
        };
        let (name, auto) = head.ok_or_else(|| EngineError::NotFound(id.to_owned()))?;
        let (mut downloaded, mut total, mut remaining) = (0, Some(0), Some(0));
        let mut running = Vec::new();
        for m in members.iter().filter(|m| m.state != State::Cancelled) {
            downloaded += m.downloaded;
            total = total.zip(m.total_size).map(|(a, b)| a + b);
            if m.state != State::Completed {
                remaining = remaining
                    .zip(m.total_size)
                    .map(|(a, b)| a + b.saturating_sub(m.downloaded));
            }
            if matches!(m.state, State::Active | State::Waiting) {
                running.extend(self.current_speed(&m.id)?);
            }
        }
        let speed = Ema::combined(running);
        Ok(GroupSnapshot {
            id: id.to_owned(),
            name,
            auto,
            members: members.into_iter().map(|m| m.id).collect(),
            downloaded,
            total,
            speed_bps: speed.speed(),
            eta_secs: speed.eta(remaining),
        })
    }

    /// A unique exact name, or a unique id prefix.
    pub fn resolve_group(&self, name_or_id: &str) -> Result<GroupId, EngineError> {
        let db = self.db();
        for sql in [
            "SELECT id FROM groups WHERE name = ?1 LIMIT 2",
            "SELECT id FROM groups WHERE substr(id, 1, length(?1)) = ?1 LIMIT 2",
        ] {
            let mut stmt = db.conn().prepare(sql)?;
            let ids: Vec<String> = stmt
                .query_map([name_or_id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            match ids.as_slice() {
                [id] => return Ok(id.clone()),
                [] => continue,
                _ => return Err(EngineError::Ambiguous(name_or_id.to_owned())),
            }
        }
        Err(EngineError::NotFound(name_or_id.to_owned()))
    }

    /// Puts a download in `group`, or in none. The user decided: `group`
    /// becomes manual and auto-grouping leaves this download alone. The
    /// group it leaves is deleted if empty, and dissolved if automatic and
    /// down to one member.
    pub fn set_group(
        &self,
        id: &str,
        group: Option<&str>,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<Snapshot, EngineError> {
        let changed = {
            let db = self.db();
            let tx = write_tx(db.conn())?;
            let before: Option<GroupId> = tx
                .query_row("SELECT group_id FROM downloads WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()?
                .ok_or_else(|| EngineError::NotFound(id.to_owned()))?;
            if let Some(g) = group
                && tx.execute("UPDATE groups SET auto = 0 WHERE id = ?1", [g])? == 0
            {
                return Err(EngineError::NotFound(g.to_owned()));
            }
            tx.execute(
                "UPDATE downloads SET group_id = ?2, group_pinned = 1 WHERE id = ?1",
                params![id, group],
            )?;
            let mut changed = vec![id.to_owned()];
            if let Some(old) = before.as_deref().filter(|&old| Some(old) != group) {
                changed.extend(tidy(&tx, old)?);
            }
            tx.commit()?;
            changed
        };
        self.announce_groups(&changed, on_event)?;
        self.stored(id)
    }

    /// Renames a group, which makes it manual.
    pub fn rename_group(
        &self,
        group: &str,
        name: &str,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<GroupSnapshot, EngineError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(EngineError::InvalidName(name.to_owned()));
        }
        let updated = self.db().conn().execute(
            "UPDATE groups SET name = ?2, auto = 0 WHERE id = ?1",
            params![group, name],
        )?;
        if updated == 0 {
            return Err(EngineError::NotFound(group.to_owned()));
        }
        let snap = self.group(group)?;
        for id in &snap.members {
            on_event(EngineEvent::GroupChanged {
                id: id.clone(),
                group: Some(snap.clone()),
            });
        }
        Ok(snap)
    }

    /// Joins `id` to the automatic group of the downloads in its directory
    /// whose names have its key, or forms one with an ungrouped such
    /// download (§13.2). On `Add`, and on `Renamed` (M3.5): a member of an
    /// automatic group none of whose other members share its key any more
    /// leaves it first. Returns the downloads whose group changed.
    pub(super) fn auto_group(&self, id: &str) -> Result<Vec<DownloadId>, EngineError> {
        let db = self.db();
        let tx = write_tx(db.conn())?;
        let all = members(&tx)?;
        let me_at = all
            .iter()
            .position(|m| m.id == id)
            .ok_or_else(|| EngineError::NotFound(id.to_owned()))?;
        let me = &all[me_at];
        if me.pinned || (me.group.is_some() && !me.auto) {
            return Ok(Vec::new());
        }
        let mine = naming::group_key(&me.name);
        // Same directory and key, oldest first, with the display name of
        // whichever is older.
        let peers: Vec<(&Member, String)> = all
            .iter()
            .enumerate()
            .filter(|(_, m)| m.id != id && m.dir == me.dir)
            .filter_map(|(at, m)| {
                let (mine, my_name) = mine.as_ref()?;
                let (key, name) = naming::group_key(&m.name)?;
                (key == *mine).then(|| (m, if at < me_at { name } else { my_name.clone() }))
            })
            .collect();
        let mut changed = Vec::new();
        if let Some(g) = &me.group {
            if peers.iter().any(|(m, _)| m.group.as_ref() == Some(g)) {
                return Ok(changed);
            }
            tx.execute("UPDATE downloads SET group_id = NULL WHERE id = ?1", [id])?;
            changed.push(id.to_owned());
            changed.extend(tidy(&tx, g)?);
        }
        if let Some((m, _)) = peers.iter().find(|(m, _)| m.group.is_some() && m.auto) {
            tx.execute(
                "UPDATE downloads SET group_id = ?2 WHERE id = ?1",
                params![id, m.group],
            )?;
            changed.push(id.to_owned());
        } else if let Some((m, name)) = peers.iter().find(|(m, _)| m.group.is_none() && !m.pinned) {
            let group = uuid::Uuid::now_v7().to_string();
            tx.execute(
                "INSERT INTO groups (id, name, auto) VALUES (?1, ?2, 1)",
                params![group, name],
            )?;
            tx.execute(
                "UPDATE downloads SET group_id = ?3 WHERE id IN (?1, ?2)",
                params![m.id, id, group],
            )?;
            changed.extend([m.id.clone(), id.to_owned()]);
        }
        tx.commit()?;
        let mut seen = std::collections::HashSet::new();
        changed.retain(|c| seen.insert(c.clone()));
        Ok(changed)
    }

    /// `GroupChanged` for each of `ids`, with its group as it is now.
    pub(super) fn announce_groups(
        &self,
        ids: &[DownloadId],
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<(), EngineError> {
        for id in ids {
            let group = self.stored(id)?.group.map(|g| self.group(&g)).transpose()?;
            on_event(EngineEvent::GroupChanged {
                id: id.clone(),
                group,
            });
        }
        Ok(())
    }

    /// EMA of the download's last seconds of 1 s samples, or `None` if it
    /// has no recent ones.
    fn current_speed(&self, id: &str) -> Result<Option<Ema>, EngineError> {
        let db = self.db();
        let mut stmt = db.conn().prepare_cached(
            "SELECT at, bytes_per_sec FROM speed_samples
             WHERE download_id = ?1 AND period_secs = 1 AND at >= ?2 ORDER BY at DESC",
        )?;
        let now = now_ms();
        let samples: Vec<(i64, i64)> = stmt
            .query_map(params![id, now - RECENT_MS], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        match samples.first() {
            Some(&(at, _)) if now - at <= STALE_MS => {}
            _ => return Ok(None),
        }
        let mut ema = Ema::default();
        for &(_, bps) in samples.iter().rev() {
            ema.push(bps as f64);
        }
        Ok(Some(ema))
    }
}

/// Takes the write lock up front: a deferred transaction that reads, then
/// writes after another process committed, fails with `SQLITE_BUSY` without
/// waiting.
fn write_tx(conn: &Connection) -> rusqlite::Result<Transaction<'_>> {
    Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
}

/// Every download with what grouping needs, oldest first.
fn members(conn: &Connection) -> Result<Vec<Member>, EngineError> {
    let mut stmt = conn.prepare(
        "SELECT d.id, d.final_path, d.group_id, g.auto, d.group_pinned
         FROM downloads d LEFT JOIN groups g ON g.id = d.group_id
         ORDER BY d.created_at, d.id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let path = PathBuf::from(r.get::<_, String>(1)?);
            Ok(Member {
                id: r.get(0)?,
                dir: path.parent().map(PathBuf::from).unwrap_or_default(),
                name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                group: r.get(2)?,
                auto: r.get::<_, Option<bool>>(3)?.unwrap_or(false),
                pinned: r.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// After a member left `group`: deletes it if empty, dissolves it if
/// automatic with one member left. Returns the downloads that left too.
/// `Remove` must call it for the removed download's group.
fn tidy(conn: &Connection, group: &str) -> Result<Vec<DownloadId>, EngineError> {
    let auto: bool = conn.query_row("SELECT auto FROM groups WHERE id = ?1", [group], |r| {
        r.get(0)
    })?;
    let mut stmt = conn.prepare("SELECT id FROM downloads WHERE group_id = ?1")?;
    let left: Vec<DownloadId> = stmt
        .query_map([group], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if !left.is_empty() && !(auto && left.len() == 1) {
        return Ok(Vec::new());
    }
    conn.execute(
        "UPDATE downloads SET group_id = NULL WHERE group_id = ?1",
        [group],
    )?;
    conn.execute("DELETE FROM groups WHERE id = ?1", [group])?;
    Ok(left)
}
