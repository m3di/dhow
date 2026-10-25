-- Schema v4: added in blind mode (ENGINE-SPEC §2.4): its probe failed and
-- the user chose to download anyway, so a failed probe on a later run
-- doesn't fail it either.
ALTER TABLE downloads ADD COLUMN blind INTEGER NOT NULL DEFAULT 0
    CHECK (blind IN (0, 1));
