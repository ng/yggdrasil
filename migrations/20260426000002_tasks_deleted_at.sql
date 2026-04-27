-- yggdrasil-123: soft-delete + trash for tasks. ygg task delete sets
-- deleted_at; ygg task restore clears it; ygg task purge hard-deletes
-- rows where deleted_at < now() - N days.

ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;

-- Partial index on the trash so the common case (live tasks) ignores it
-- entirely while `ygg task trash` and the purge query stay cheap.
CREATE INDEX IF NOT EXISTS idx_tasks_deleted_at
    ON tasks (deleted_at)
    WHERE deleted_at IS NOT NULL;
