-- Workers lifecycle table (yggdrasil-50).
-- A worker is a spawned CC session executing a specific task inside a
-- worktree. We track it explicitly so we can tell "alive and thinking"
-- from "tmux killed on restart" from "claude exited without closing
-- the task" — the tasks.status='in_progress' proxy can't distinguish.

CREATE TYPE worker_state AS ENUM (
    'spawned',          -- row written, tmux fired, observer hasn't confirmed yet
    'running',          -- observer saw claude prompt / active use
    'idle',             -- window present, no activity in the poll window
    'needs_attention',  -- observer scraped a prompt we can't auto-answer
    'completed',        -- claude exited cleanly and task closed
    'failed',           -- claude exited non-zero or task closed with 'fail'
    'abandoned'         -- tmux window absent; likely machine restart or manual kill
);

CREATE TABLE workers (
    worker_id     UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    task_id       UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    session_id    UUID REFERENCES sessions(session_id) ON DELETE SET NULL,
    tmux_session  TEXT NOT NULL,
    tmux_window   TEXT NOT NULL,
    worktree_path TEXT NOT NULL,
    state         worker_state NOT NULL DEFAULT 'spawned',
    started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at      TIMESTAMPTZ,
    exit_reason   TEXT
);

CREATE INDEX idx_workers_task  ON workers (task_id, started_at DESC);
CREATE INDEX idx_workers_live  ON workers (tmux_session, tmux_window)
    WHERE ended_at IS NULL;
CREATE INDEX idx_workers_state ON workers (state, started_at DESC);
