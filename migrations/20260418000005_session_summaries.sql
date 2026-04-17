-- Session summaries: one durable row per Claude Code session.
-- Finalized by the Stop hook and by the watcher (for crashed sessions).
-- Queries over time windows should prefer this table over aggregating the
-- events firehose on every call.

CREATE TABLE session_summaries (
    session_id     UUID PRIMARY KEY REFERENCES sessions(session_id),
    agent_id       UUID NOT NULL REFERENCES agents(agent_id),
    agent_name     TEXT NOT NULL,
    repo_id        UUID REFERENCES repos(repo_id),
    repo_prefix    TEXT,                       -- denormalized for cheap filtering
    started_at     TIMESTAMPTZ NOT NULL,
    ended_at       TIMESTAMPTZ,                -- NULL = still active

    -- Retrieval
    user_prompts        INT NOT NULL DEFAULT 0,
    similarity_hits     INT NOT NULL DEFAULT 0,
    scoring_drops       INT NOT NULL DEFAULT 0,
    classifier_kept     INT NOT NULL DEFAULT 0,
    classifier_dropped  INT NOT NULL DEFAULT 0,
    classifier_bypassed INT NOT NULL DEFAULT 0,
    embedding_calls     INT NOT NULL DEFAULT 0,
    cache_hits          INT NOT NULL DEFAULT 0,
    ollama_ms_sum       BIGINT NOT NULL DEFAULT 0,

    -- Context
    nodes_written    INT NOT NULL DEFAULT 0,
    digests_written  INT NOT NULL DEFAULT 0,
    max_context_tokens INT,

    -- Coordination
    locks_acquired   INT NOT NULL DEFAULT 0,
    locks_released   INT NOT NULL DEFAULT 0,
    lock_conflicts   INT NOT NULL DEFAULT 0,
    interrupts       INT NOT NULL DEFAULT 0,

    -- Work
    tasks_created  INT NOT NULL DEFAULT 0,
    tasks_closed   INT NOT NULL DEFAULT 0,
    remembers      INT NOT NULL DEFAULT 0,

    finalized_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_session_summaries_agent  ON session_summaries (agent_id, started_at DESC);
CREATE INDEX idx_session_summaries_repo   ON session_summaries (repo_id, started_at DESC);
CREATE INDEX idx_session_summaries_start  ON session_summaries (started_at DESC);
