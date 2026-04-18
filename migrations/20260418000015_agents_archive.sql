-- Let agents retire without losing their history. archived_at IS NULL means
-- the agent is live; a non-null value hides them from live views while
-- preserving nodes/events/sessions for retrieval + audit.

ALTER TABLE agents ADD COLUMN IF NOT EXISTS archived_at TIMESTAMPTZ;
CREATE INDEX IF NOT EXISTS idx_agents_active
    ON agents (updated_at DESC)
    WHERE archived_at IS NULL;
