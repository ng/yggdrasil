ALTER TABLE events ADD COLUMN IF NOT EXISTS cc_session_id TEXT;
CREATE INDEX IF NOT EXISTS idx_events_cc_session ON events (cc_session_id, created_at DESC)
    WHERE cc_session_id IS NOT NULL;
