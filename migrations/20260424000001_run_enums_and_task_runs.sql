-- ADR 0016: autonomous execution. State machine for individual task
-- attempts (distinct from tasks.status which stays semantic).
CREATE TYPE run_state AS ENUM (
    'scheduled',
    'ready',
    'running',
    'succeeded',
    'failed',
    'crashed',
    'cancelled',
    'retrying',
    'poison'
);

CREATE TYPE run_reason AS ENUM (
    'ok',
    'agent_error',
    'heartbeat_timeout',
    'tmux_gone',
    'max_attempts',
    'user_cancelled',
    'dependency_failed',
    'lock_conflict',
    'timeout',
    'loop_detected',
    'budget_exceeded'
);

-- One row per execution attempt. DBOS-shaped checkpoint: scheduler is
-- the only writer of `state`; `Stop` hook writes outcome fields, the
-- scheduler reconciles them on its next tick.
CREATE TABLE task_runs (
    run_id          UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    task_id         UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    attempt         INT  NOT NULL,
    parent_run_id   UUID REFERENCES task_runs(run_id),

    -- Structural idempotency key: "run:<task_id>:attempt:<n>". Never derived
    -- from agent output (LLMs are non-deterministic).
    idempotency_key TEXT NOT NULL,

    state           run_state  NOT NULL DEFAULT 'scheduled',
    reason          run_reason NOT NULL DEFAULT 'ok',

    -- Lifecycle timestamps.
    scheduled_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    claimed_at      TIMESTAMPTZ,
    started_at      TIMESTAMPTZ,
    ended_at        TIMESTAMPTZ,
    heartbeat_at    TIMESTAMPTZ,
    heartbeat_ttl_s INT NOT NULL DEFAULT 300,

    -- Execution binding (filled in on claim / spawn).
    agent_id        UUID REFERENCES agents(agent_id),
    worker_id       UUID,                            -- workers FK lives in click-to-do
    session_id      UUID REFERENCES sessions(session_id),

    -- Retry metadata.
    max_attempts    INT NOT NULL DEFAULT 3,
    retry_strategy  JSONB NOT NULL DEFAULT
        '{"kind":"exponential","base_ms":60000,"cap_ms":600000,"jitter":true}',
    deadline_at     TIMESTAMPTZ,

    -- Inline payloads. Discipline: reject > 16 KiB in Rust; oversize content
    -- diverts to blob store.
    input           JSONB NOT NULL DEFAULT '{}',
    output          JSONB,
    error           JSONB,

    -- Claim-check pointers for large payloads.
    output_commit_sha TEXT,
    output_branch     TEXT,
    output_pr_url     TEXT,
    output_worktree   TEXT,
    output_blob_ref   TEXT,

    -- Loop-detection fingerprint (sha256 hex).
    fingerprint     TEXT,

    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (task_id, attempt),
    UNIQUE (idempotency_key),
    CHECK (attempt >= 1),
    CHECK (max_attempts >= 1)
);

-- Hot path: scheduler scans ready runs ordered by task priority + scheduled age.
CREATE INDEX idx_runs_ready
    ON task_runs (scheduled_at)
    WHERE state = 'ready';

-- Heartbeat reconciler: runs with expired heartbeats need crashing.
CREATE INDEX idx_runs_live_heartbeat
    ON task_runs (heartbeat_at)
    WHERE state = 'running';

-- Retry math.
CREATE INDEX idx_runs_retry_candidates
    ON task_runs (ended_at)
    WHERE state IN ('failed', 'crashed');

-- Deadline enforcement.
CREATE INDEX idx_runs_deadline
    ON task_runs (deadline_at)
    WHERE state = 'running' AND deadline_at IS NOT NULL;

-- Per-task history readout (`ygg task show`).
CREATE INDEX idx_runs_task     ON task_runs (task_id, attempt DESC);
CREATE INDEX idx_runs_agent    ON task_runs (agent_id, started_at DESC) WHERE agent_id IS NOT NULL;
CREATE INDEX idx_runs_worker   ON task_runs (worker_id) WHERE worker_id IS NOT NULL;
