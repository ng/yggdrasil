-- Split agent identity from CC session state (yggdrasil-30).
--
-- Parallel CC sessions in the same repo raced on agents.current_state /
-- head_node_id / context_tokens, overwriting each other. Per-session rows
-- track live state; agents stays as the long-lived identity.

-- Map CC's own session_id onto our internal sessions row. UNIQUE so the
-- UPSERT path is safe, NULLable so legacy rows pre-dating this migration
-- survive.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS cc_session_id TEXT UNIQUE;

-- Per-session liveness. Defaults mirror agents.* so existing callers that
-- forget to update the session still behave sanely.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS current_state   agent_state NOT NULL DEFAULT 'idle';
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS head_node_id    UUID REFERENCES nodes(id);
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS context_tokens  INT NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS last_tool       TEXT;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS updated_at      TIMESTAMPTZ NOT NULL DEFAULT now();

-- Events pick up a session_id FK so retrieval/analytics can scope by session
-- without joining through cc_session_id twice.
ALTER TABLE events ADD COLUMN IF NOT EXISTS session_id UUID REFERENCES sessions(session_id);
CREATE INDEX IF NOT EXISTS idx_events_session ON events (session_id, created_at DESC)
    WHERE session_id IS NOT NULL;

-- Live sessions lookup — "who's active right now per agent".
CREATE INDEX IF NOT EXISTS idx_sessions_live
    ON sessions (agent_id, updated_at DESC)
    WHERE ended_at IS NULL;
