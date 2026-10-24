-- Schema v1. ARCHITECTURE §5.1. Times are unix milliseconds UTC; ids are
-- UUID text; JSON columns are checked with json_valid. The sidecar journal,
-- not this database, is the integrity authority (ENGINE-SPEC §6 I3).

CREATE TABLE queues (
    id             TEXT PRIMARY KEY,
    -- NULL only for the default queue: the shell shows its own localized name.
    name           TEXT,
    max_concurrent INTEGER NOT NULL DEFAULT 3 CHECK (max_concurrent > 0),
    speed_limit    INTEGER CHECK (speed_limit >= 0),          -- bytes/s, NULL = unlimited
    schedule       TEXT CHECK (schedule IS NULL OR json_valid(schedule)),
    on_complete    TEXT NOT NULL DEFAULT 'nothing'
                   CHECK (on_complete IN ('nothing', 'sleep', 'shutdown', 'hibernate')),
    is_default     INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0, 1)),
    CHECK (name IS NOT NULL OR is_default = 1),
    CHECK (schedule IS NULL OR is_default = 0)
) STRICT;

CREATE UNIQUE INDEX queues_single_default ON queues (is_default) WHERE is_default = 1;

-- ENGINE-SPEC §11: the default queue always exists and cannot be deleted.
INSERT INTO queues (id, is_default) VALUES ('00000000-0000-0000-0000-000000000000', 1);

CREATE TRIGGER queues_keep_default
BEFORE DELETE ON queues WHEN OLD.is_default = 1
BEGIN
    SELECT RAISE(ABORT, 'the default queue cannot be deleted');
END;

CREATE TABLE groups (
    id   TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    auto INTEGER NOT NULL CHECK (auto IN (0, 1))
) STRICT;

CREATE TABLE downloads (
    id              TEXT PRIMARY KEY,                          -- UUIDv7
    url             TEXT NOT NULL,
    headers         TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(headers)),
    final_path      TEXT NOT NULL,
    part_path       TEXT NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('probing', 'queued', 'waiting', 'active',
                        'paused', 'completing', 'completed', 'failed', 'cancelled')),
    fail_reason     TEXT,
    queue_id        TEXT NOT NULL REFERENCES queues (id) ON DELETE RESTRICT,
    position        INTEGER NOT NULL,                          -- order within queue (Reorder)
    group_id        TEXT REFERENCES groups (id) ON DELETE SET NULL,
    category        TEXT NOT NULL CHECK (category IN ('video', 'audio', 'image', 'document',
                        'archive', 'software', 'code', 'data', 'other')),
    created_at      INTEGER NOT NULL,
    started_at      INTEGER,
    completed_at    INTEGER,
    total_size      INTEGER CHECK (total_size >= 0),           -- NULL = blind mode
    etag            TEXT,
    last_modified   TEXT,
    digest          TEXT,                                      -- from probe, for I8 across restarts
    resumable       TEXT NOT NULL CHECK (resumable IN ('yes', 'no', 'unknown')),
    size_unverified INTEGER NOT NULL DEFAULT 0 CHECK (size_unverified IN (0, 1)),  -- I7
    identity_weak   INTEGER NOT NULL DEFAULT 0 CHECK (identity_weak IN (0, 1)),    -- I4
    connections     INTEGER CHECK (connections BETWEEN 1 AND 255),                 -- NULL = auto
    speed_limit     INTEGER CHECK (speed_limit >= 0),          -- bytes/s, NULL = unlimited
    file_token      BLOB,
    CHECK ((state = 'failed') = (fail_reason IS NOT NULL))
) STRICT;

CREATE INDEX downloads_queue_order ON downloads (queue_id, position);
CREATE INDEX downloads_group ON downloads (group_id) WHERE group_id IS NOT NULL;

-- Mirror of the sidecar for queries only; rebuilt from the sidecar on open.
CREATE TABLE ranges (
    download_id TEXT NOT NULL REFERENCES downloads (id) ON DELETE CASCADE,
    start       INTEGER NOT NULL CHECK (start >= 0),
    end         INTEGER NOT NULL,
    state       TEXT NOT NULL CHECK (state IN ('pending', 'active', 'committed')),
    PRIMARY KEY (download_id, start),
    CHECK (start < end)
) STRICT, WITHOUT ROWID;

-- download_id NULL = the global aggregate (ENGINE-SPEC §14).
-- period_secs: 1 for the 10-minute ring, 3600 for hourly rollups.
CREATE TABLE speed_samples (
    download_id   TEXT REFERENCES downloads (id) ON DELETE CASCADE,
    period_secs   INTEGER NOT NULL CHECK (period_secs IN (1, 3600)),
    at            INTEGER NOT NULL,
    bytes_per_sec INTEGER NOT NULL CHECK (bytes_per_sec >= 0)
) STRICT;

CREATE INDEX speed_samples_lookup ON speed_samples (download_id, period_secs, at);

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL CHECK (json_valid(value))
) STRICT, WITHOUT ROWID;
