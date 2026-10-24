-- Schema v3: the user chose this download's group, or no group, with
-- SetGroup (ENGINE-SPEC §13.2); auto-grouping leaves it alone.
ALTER TABLE downloads ADD COLUMN group_pinned INTEGER NOT NULL DEFAULT 0
    CHECK (group_pinned IN (0, 1));
