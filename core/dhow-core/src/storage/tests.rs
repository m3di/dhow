use rusqlite::{Connection, params};

use super::*;

fn fresh() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("dhow.sqlite")).unwrap();
    (dir, db)
}

fn insert_download(conn: &Connection, id: &str, state: &str, fail_reason: Option<&str>) {
    conn.execute(
        "INSERT INTO downloads (id, url, final_path, part_path, state, fail_reason, queue_id,
             position, category, created_at, resumable)
         VALUES (?1, 'http://x/f', '/d/f', '/d/f.part', ?2, ?3, ?4, 0, 'other', 0, 'yes')",
        params![id, state, fail_reason, DEFAULT_QUEUE_ID],
    )
    .map(|_| ())
    .unwrap_or_else(|e| panic!("insert {id}: {e}"));
}

fn try_exec(conn: &Connection, sql: &str) -> Result<usize, rusqlite::Error> {
    conn.execute(sql, [])
}

#[test]
fn fresh_database_is_at_latest_version_with_pragmas() {
    let (_dir, db) = fresh();
    assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    let conn = db.conn();
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let sync: i64 = conn
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(sync, 1, "NORMAL");
    let fk: bool = conn
        .pragma_query_value(None, "foreign_keys", |r| r.get(0))
        .unwrap();
    assert!(fk);
}

#[test]
fn has_every_table() {
    let (_dir, db) = fresh();
    let mut stmt = db
        .conn()
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .unwrap();
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        tables,
        [
            "downloads",
            "groups",
            "queues",
            "ranges",
            "settings",
            "speed_samples"
        ]
    );
}

#[test]
fn reopen_keeps_data_and_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/dir/dhow.sqlite");
    let db = Db::open(&path).unwrap();
    insert_download(db.conn(), "a", "queued", None);
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    let n: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM downloads", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn newer_schema_is_refused_and_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dhow.sqlite");
    drop(Db::open(&path).unwrap());
    let future = SCHEMA_VERSION + 5;
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", future)
        .unwrap();
    assert!(matches!(
        Db::open(&path),
        Err(StorageError::TooNew { found, supported })
            if found == future && supported == SCHEMA_VERSION
    ));
    let v: u32 = Connection::open(&path)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, future);
}

#[test]
fn failed_migration_rolls_back_entirely() {
    let mut conn = Connection::open_in_memory().unwrap();
    let migrations = [
        "CREATE TABLE a (x INTEGER);",
        "CREATE TABLE b (x INTEGER); INSERT INTO nope VALUES (1);",
    ];
    let err = migrate(&mut conn, &migrations).unwrap_err();
    assert!(
        matches!(err, StorageError::Migration { version: 2, .. }),
        "{err}"
    );
    assert_eq!(user_version(&conn).unwrap(), 1);
    let b: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(b, 0, "partial migration 2 must not survive");
    // Fixing the migration and retrying picks up where it stopped.
    migrate(
        &mut conn,
        &["CREATE TABLE a (x INTEGER);", "CREATE TABLE b (x INTEGER);"],
    )
    .unwrap();
    assert_eq!(user_version(&conn).unwrap(), 2);
}

#[test]
fn default_queue_exists_and_cannot_be_deleted() {
    let (_dir, db) = fresh();
    let conn = db.conn();
    let (id, name): (String, Option<String>) = conn
        .query_row(
            "SELECT id, name FROM queues WHERE is_default = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(id, DEFAULT_QUEUE_ID);
    assert_eq!(name, None, "the shell localizes the default queue's name");
    let err = try_exec(conn, "DELETE FROM queues").unwrap_err();
    assert!(err.to_string().contains("default queue"), "{err}");
}

#[test]
fn only_one_default_queue_and_only_it_may_be_unnamed() {
    let (_dir, db) = fresh();
    let conn = db.conn();
    assert!(
        try_exec(
            conn,
            "INSERT INTO queues (id, name, is_default) VALUES ('q', 'Q', 1)"
        )
        .is_err()
    );
    assert!(try_exec(conn, "INSERT INTO queues (id) VALUES ('q')").is_err());
    try_exec(conn, "INSERT INTO queues (id, name) VALUES ('q', 'Night')").unwrap();
}

#[test]
fn failed_state_and_fail_reason_go_together() {
    let (_dir, db) = fresh();
    let conn = db.conn();
    insert_download(conn, "ok-failed", "failed", Some("transient"));
    insert_download(conn, "ok-active", "active", None);
    let bad = |id: &str, state: &str, reason: &str| {
        try_exec(
            conn,
            &format!(
                "INSERT INTO downloads (id, url, final_path, part_path, state, fail_reason,
                     queue_id, position, category, created_at, resumable)
                 VALUES ('{id}', 'u', 'f', 'p', '{state}', {reason}, '{DEFAULT_QUEUE_ID}',
                     0, 'other', 0, 'yes')"
            ),
        )
        .is_err()
    };
    assert!(bad("x1", "failed", "NULL"));
    assert!(bad("x2", "active", "'transient'"));
    assert!(bad("x3", "exploded", "NULL"));
}

#[test]
fn queue_with_downloads_cannot_be_deleted() {
    let (_dir, db) = fresh();
    let conn = db.conn();
    try_exec(conn, "INSERT INTO queues (id, name) VALUES ('q', 'Night')").unwrap();
    insert_download(conn, "a", "queued", None);
    try_exec(conn, "UPDATE downloads SET queue_id = 'q'").unwrap();
    assert!(try_exec(conn, "DELETE FROM queues WHERE id = 'q'").is_err());
    try_exec(
        conn,
        &format!("UPDATE downloads SET queue_id = '{DEFAULT_QUEUE_ID}'"),
    )
    .unwrap();
    try_exec(conn, "DELETE FROM queues WHERE id = 'q'").unwrap();
}

#[test]
fn removing_a_download_removes_its_ranges_and_samples() {
    let (_dir, db) = fresh();
    let conn = db.conn();
    insert_download(conn, "a", "active", None);
    try_exec(conn, "INSERT INTO ranges VALUES ('a', 0, 100, 'committed')").unwrap();
    try_exec(conn, "INSERT INTO speed_samples VALUES ('a', 1, 0, 10)").unwrap();
    try_exec(conn, "INSERT INTO speed_samples VALUES (NULL, 1, 0, 10)").unwrap();
    try_exec(conn, "DELETE FROM downloads").unwrap();
    let left: (i64, i64) = conn
        .query_row(
            "SELECT (SELECT count(*) FROM ranges), (SELECT count(*) FROM speed_samples)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(left, (0, 1), "global aggregate sample survives");
}

#[test]
fn ranges_must_be_non_empty_and_reference_a_download() {
    let (_dir, db) = fresh();
    let conn = db.conn();
    insert_download(conn, "a", "active", None);
    assert!(try_exec(conn, "INSERT INTO ranges VALUES ('a', 5, 5, 'pending')").is_err());
    assert!(try_exec(conn, "INSERT INTO ranges VALUES ('nope', 0, 5, 'pending')").is_err());
}

#[test]
fn json_columns_reject_garbage_and_strict_rejects_wrong_types() {
    let (_dir, db) = fresh();
    let conn = db.conn();
    assert!(try_exec(conn, "INSERT INTO settings VALUES ('k', '{nope')").is_err());
    try_exec(conn, "INSERT INTO settings VALUES ('k', '{\"a\":1}')").unwrap();
    insert_download(conn, "a", "active", None);
    assert!(try_exec(conn, "UPDATE downloads SET headers = 'nope'").is_err());
    assert!(try_exec(conn, "UPDATE downloads SET total_size = 'big'").is_err());
}

#[test]
fn v1_database_migrates_to_v2_keeping_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dhow.sqlite");
    {
        let mut conn = Connection::open(&path).unwrap();
        configure(&conn).unwrap();
        migrate(&mut conn, &MIGRATIONS[..1]).unwrap();
        conn.execute("INSERT INTO queues (id, name) VALUES ('q', 'Night')", [])
            .unwrap();
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 2);
    let (state, until): (String, Option<i64>) = db
        .conn()
        .query_row(
            "SELECT run_state, run_state_until FROM queues WHERE id = 'q'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), until), ("auto", None));
    assert!(
        db.conn()
            .execute("UPDATE queues SET run_state = 'paused'", [])
            .is_err()
    );
}
