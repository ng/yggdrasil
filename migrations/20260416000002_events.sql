CREATE TYPE event_kind AS ENUM (
    'node_written',
    'lock_acquired',
    'lock_released',
    'digest_written',
    'similarity_hit',
    'correction_detected',
    'hook_fired'
);

CREATE TABLE events (
    id          UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    event_kind  event_kind NOT NULL,
    agent_id    UUID,
    agent_name  TEXT NOT NULL DEFAULT '',
    payload     JSONB NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_events_created ON events (created_at DESC);
CREATE INDEX idx_events_agent   ON events (agent_id, created_at DESC);
