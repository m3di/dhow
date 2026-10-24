-- Schema v2: StartQueue / StopQueue overrides (ENGINE-SPEC §11).
-- 'auto' follows the schedule (always on without one); 'started' and
-- 'stopped' override it until run_state_until (unix ms, the next schedule
-- edge when set), or indefinitely when that is NULL.
ALTER TABLE queues ADD COLUMN run_state TEXT NOT NULL DEFAULT 'auto'
    CHECK (run_state IN ('auto', 'started', 'stopped'));
ALTER TABLE queues ADD COLUMN run_state_until INTEGER;
