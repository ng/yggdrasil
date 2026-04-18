-- External reference for each task — gh-123, jira-PROJ-456, or a full URL.
-- Enables round-tripping with GitHub issues / Jira / etc without stuffing
-- the link into the description body.

ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS external_ref TEXT;

CREATE INDEX IF NOT EXISTS idx_tasks_external_ref
    ON tasks (external_ref)
    WHERE external_ref IS NOT NULL;
