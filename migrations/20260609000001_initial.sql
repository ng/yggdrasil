-- Squashed baseline schema for Yggdrasil.
--
-- ADR 0015: the embedding / similarity-retrieval layer (nodes, memories,
-- embedding_cache, pgvector) was removed. This migration creates the final
-- schema from scratch — no pgvector extension required.

CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

---------------------------------------------------------------------
-- LOCKS: Semantic leases
---------------------------------------------------------------------
CREATE TABLE locks (
    id            UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    resource_key  TEXT NOT NULL,
    agent_id      UUID NOT NULL,
    acquired_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at    TIMESTAMPTZ NOT NULL,
    heartbeat_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    user_id       TEXT NOT NULL DEFAULT '',
    CONSTRAINT uq_lock_resource UNIQUE (resource_key)
);

CREATE INDEX idx_locks_agent ON locks (agent_id);
CREATE INDEX idx_locks_expiry ON locks (expires_at);
CREATE INDEX idx_locks_user ON locks (user_id);

---------------------------------------------------------------------
-- AGENTS: Workflow state machine
---------------------------------------------------------------------
CREATE TYPE agent_state AS ENUM (
    'idle',
    'planning',
    'executing',
    'waiting_tool',
    'context_flush',
    'human_override',
    'mediation',
    'error',
    'shutdown'
);

CREATE TABLE agents (
    agent_id        UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    agent_name      TEXT NOT NULL,
    current_state   agent_state NOT NULL DEFAULT 'idle',
    context_tokens  INT NOT NULL DEFAULT 0,
    metadata        JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    persona         TEXT,
    archived_at     TIMESTAMPTZ,
    user_id         TEXT NOT NULL DEFAULT '',
    message_cursor  TIMESTAMPTZ DEFAULT '1970-01-01'::timestamptz
);

CREATE UNIQUE INDEX agents_name_persona_user_uk
    ON agents (user_id, agent_name, COALESCE(persona, ''));
CREATE INDEX idx_agents_active ON agents (updated_at DESC) WHERE archived_at IS NULL;
CREATE INDEX idx_agents_user ON agents (user_id) WHERE archived_at IS NULL;

---------------------------------------------------------------------
-- AGENT_STATS: Token usage rollups
---------------------------------------------------------------------
CREATE TABLE agent_stats (
    id              UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    agent_id        UUID NOT NULL REFERENCES agents(agent_id),
    period          TIMESTAMPTZ NOT NULL,
    input_tokens    BIGINT NOT NULL DEFAULT 0,
    output_tokens   BIGINT NOT NULL DEFAULT 0,
    cache_read      BIGINT NOT NULL DEFAULT 0,
    cache_write     BIGINT NOT NULL DEFAULT 0,
    tool_calls      INT NOT NULL DEFAULT 0,
    task_category   TEXT,
    estimated_cost  NUMERIC(10,6) NOT NULL DEFAULT 0,
    UNIQUE(agent_id, period, task_category)
);

---------------------------------------------------------------------
-- EVENTS
---------------------------------------------------------------------
CREATE TYPE event_kind AS ENUM (
    'lock_acquired',
    'lock_released',
    'hook_fired',
    'task_created',
    'task_status_changed',
    'agent_state_changed',
    'message',
    'run_scheduled',
    'run_claimed',
    'run_terminal',
    'run_retry',
    'scheduler_tick',
    'scheduler_error',
    'agent_stale_warning'
);

CREATE TABLE events (
    id                  UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    event_kind          event_kind NOT NULL,
    agent_id            UUID,
    agent_name          TEXT NOT NULL DEFAULT '',
    payload             JSONB NOT NULL DEFAULT '{}',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    cc_session_id       TEXT,
    session_id          UUID,
    recipient_agent_id  UUID REFERENCES agents(agent_id),
    user_id             TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_events_created ON events (created_at DESC);
CREATE INDEX idx_events_agent ON events (agent_id, created_at DESC);
CREATE INDEX idx_events_cc_session ON events (cc_session_id, created_at DESC)
    WHERE cc_session_id IS NOT NULL;
CREATE INDEX idx_events_session ON events (session_id, created_at DESC)
    WHERE session_id IS NOT NULL;
CREATE INDEX idx_events_recipient_unread
    ON events (recipient_agent_id, created_at DESC)
    WHERE recipient_agent_id IS NOT NULL;
CREATE INDEX idx_events_message_created
    ON events (created_at DESC)
    WHERE event_kind = 'message';
CREATE INDEX idx_events_user ON events (user_id);

---------------------------------------------------------------------
-- REPOS
---------------------------------------------------------------------
CREATE TABLE repos (
    repo_id       UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    canonical_url TEXT,
    name          TEXT NOT NULL,
    task_prefix   TEXT NOT NULL,
    local_paths   TEXT[] NOT NULL DEFAULT '{}',
    metadata      JSONB NOT NULL DEFAULT '{}',
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    user_id       TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_repos_prefix ON repos (task_prefix);
CREATE UNIQUE INDEX repos_user_prefix_uk ON repos (user_id, task_prefix);
CREATE INDEX idx_repos_user ON repos (user_id);

---------------------------------------------------------------------
-- SESSIONS
---------------------------------------------------------------------
CREATE TABLE sessions (
    session_id      UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    agent_id        UUID NOT NULL REFERENCES agents(agent_id),
    repo_id         UUID REFERENCES repos(repo_id),
    cc_session_id   TEXT UNIQUE,
    current_state   agent_state NOT NULL DEFAULT 'idle',
    context_tokens  INT NOT NULL DEFAULT 0,
    last_tool       TEXT,
    started_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at        TIMESTAMPTZ,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    metadata        JSONB NOT NULL DEFAULT '{}',
    user_id         TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_sessions_agent ON sessions (agent_id, started_at DESC);
CREATE INDEX idx_sessions_repo ON sessions (repo_id, started_at DESC);
CREATE INDEX idx_sessions_live ON sessions (agent_id, updated_at DESC) WHERE ended_at IS NULL;
CREATE INDEX idx_sessions_user ON sessions (user_id, agent_id);

-- FK from events back to sessions (deferred because sessions depends on agents
-- which events also references).
ALTER TABLE events ADD CONSTRAINT events_session_id_fkey
    FOREIGN KEY (session_id) REFERENCES sessions(session_id);

---------------------------------------------------------------------
-- TASKS
---------------------------------------------------------------------
CREATE TYPE task_status AS ENUM (
    'open', 'in_progress', 'blocked', 'closed',
    'awaiting_children', 'awaiting_approval', 'awaiting_review'
);
CREATE TYPE task_kind AS ENUM ('task', 'bug', 'feature', 'chore', 'epic');

CREATE TABLE tasks (
    task_id             UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    repo_id             UUID NOT NULL REFERENCES repos(repo_id) ON DELETE CASCADE,
    seq                 INT NOT NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    acceptance          TEXT,
    design              TEXT,
    notes               TEXT,
    kind                task_kind NOT NULL DEFAULT 'task',
    status              task_status NOT NULL DEFAULT 'open',
    priority            SMALLINT NOT NULL DEFAULT 2 CHECK (priority BETWEEN 0 AND 4),
    created_by          UUID REFERENCES agents(agent_id),
    assignee            UUID REFERENCES agents(agent_id),
    human_flag          BOOLEAN NOT NULL DEFAULT FALSE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at           TIMESTAMPTZ,
    close_reason        TEXT,
    relevance           INT NOT NULL DEFAULT 50,
    external_ref        TEXT,
    deleted_at          TIMESTAMPTZ,
    user_id             TEXT NOT NULL DEFAULT '',
    -- Execution columns (ADR 0016)
    runnable            BOOLEAN NOT NULL DEFAULT FALSE,
    current_attempt_id  UUID,
    max_attempts        INT NOT NULL DEFAULT 3,
    timeout_ms          BIGINT,
    deadline_at         TIMESTAMPTZ,
    approval_level      TEXT NOT NULL DEFAULT 'auto',
    approved_at         TIMESTAMPTZ,
    approved_by_agent_id UUID REFERENCES agents(agent_id),
    parent_task_id      UUID REFERENCES tasks(task_id),
    input_spec          JSONB NOT NULL DEFAULT '{}',
    output_spec         JSONB NOT NULL DEFAULT '{}',
    agent_role          TEXT,
    required_locks      TEXT[] NOT NULL DEFAULT '{}',
    result_blob_ref     TEXT,
    plan_strategy       TEXT,
    UNIQUE (repo_id, seq),
    CONSTRAINT tasks_relevance_range CHECK (relevance BETWEEN 0 AND 100),
    CONSTRAINT tasks_approval_level_chk CHECK (approval_level IN ('auto', 'approve_plan', 'approve_completion')),
    CONSTRAINT tasks_agent_role_chk CHECK (agent_role IS NULL OR agent_role IN ('planner', 'executor', 'critic')),
    CONSTRAINT tasks_plan_strategy_chk CHECK (plan_strategy IS NULL OR plan_strategy IN ('llm'))
);

CREATE INDEX idx_tasks_repo_status ON tasks (repo_id, status, priority);
CREATE INDEX idx_tasks_assignee ON tasks (assignee) WHERE assignee IS NOT NULL;
CREATE INDEX idx_tasks_external_ref ON tasks (external_ref) WHERE external_ref IS NOT NULL;
CREATE INDEX idx_tasks_deleted_at ON tasks (deleted_at) WHERE deleted_at IS NOT NULL;
CREATE INDEX idx_tasks_user ON tasks (user_id, repo_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_tasks_runnable ON tasks (repo_id, priority, updated_at)
    WHERE runnable = TRUE AND status IN ('open', 'in_progress');
CREATE INDEX idx_tasks_parent ON tasks (parent_task_id) WHERE parent_task_id IS NOT NULL;

-- Deferred FK for current_attempt_id (task_runs defined below).
-- Added after task_runs CREATE TABLE.

-- Task dependencies
CREATE TABLE task_deps (
    task_id     UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    blocker_id  UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, blocker_id),
    CHECK (task_id <> blocker_id)
);
CREATE INDEX idx_task_deps_blocker ON task_deps (blocker_id);

-- Labels
CREATE TABLE task_labels (
    task_id  UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    label    TEXT NOT NULL,
    PRIMARY KEY (task_id, label)
);
CREATE INDEX idx_task_labels_label ON task_labels (label);

-- Task audit trail
CREATE TABLE task_events (
    event_id    UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    task_id     UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    agent_id    UUID REFERENCES agents(agent_id),
    kind        TEXT NOT NULL,
    payload     JSONB NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    run_id      UUID
);
CREATE INDEX idx_task_events_task ON task_events (task_id, created_at DESC);

-- Per-repo sequence counter
CREATE TABLE task_seq (
    repo_id   UUID PRIMARY KEY REFERENCES repos(repo_id) ON DELETE CASCADE,
    next_seq  INT NOT NULL DEFAULT 1
);

-- Task links (non-blocking relationships)
CREATE TYPE task_link_kind AS ENUM ('see_also', 'superseded_by', 'duplicate_of', 'related');

CREATE TABLE task_links (
    task_id       UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    target_id     UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    kind          task_link_kind NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, target_id, kind)
);
CREATE INDEX idx_task_links_task ON task_links (task_id);
CREATE INDEX idx_task_links_target ON task_links (target_id);

---------------------------------------------------------------------
-- SESSION SUMMARIES
---------------------------------------------------------------------
CREATE TABLE session_summaries (
    session_id          UUID PRIMARY KEY REFERENCES sessions(session_id),
    agent_id            UUID NOT NULL REFERENCES agents(agent_id),
    agent_name          TEXT NOT NULL,
    repo_id             UUID REFERENCES repos(repo_id),
    repo_prefix         TEXT,
    started_at          TIMESTAMPTZ NOT NULL,
    ended_at            TIMESTAMPTZ,

    user_prompts        INT NOT NULL DEFAULT 0,
    max_context_tokens  INT,

    -- Coordination
    locks_acquired      INT NOT NULL DEFAULT 0,
    locks_released      INT NOT NULL DEFAULT 0,
    lock_conflicts      INT NOT NULL DEFAULT 0,
    interrupts          INT NOT NULL DEFAULT 0,

    -- Work
    tasks_created       INT NOT NULL DEFAULT 0,
    tasks_closed        INT NOT NULL DEFAULT 0,

    finalized_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_session_summaries_agent ON session_summaries (agent_id, started_at DESC);
CREATE INDEX idx_session_summaries_repo ON session_summaries (repo_id, started_at DESC);
CREATE INDEX idx_session_summaries_start ON session_summaries (started_at DESC);

---------------------------------------------------------------------
-- WORKERS
---------------------------------------------------------------------
CREATE TYPE worker_state AS ENUM (
    'spawned',
    'running',
    'idle',
    'needs_attention',
    'completed',
    'failed',
    'abandoned'
);

CREATE TABLE workers (
    worker_id           UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    task_id             UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    session_id          UUID REFERENCES sessions(session_id) ON DELETE SET NULL,
    tmux_session        TEXT NOT NULL,
    tmux_window         TEXT NOT NULL,
    worktree_path       TEXT NOT NULL,
    state               worker_state NOT NULL DEFAULT 'spawned',
    started_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at            TIMESTAMPTZ,
    exit_reason         TEXT,
    branch_pushed       BOOLEAN NOT NULL DEFAULT false,
    branch_merged       BOOLEAN NOT NULL DEFAULT false,
    pr_url              TEXT,
    delivery_checked_at TIMESTAMPTZ,
    intent              TEXT,
    user_id             TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_workers_task ON workers (task_id, started_at DESC);
CREATE INDEX idx_workers_live ON workers (tmux_session, tmux_window) WHERE ended_at IS NULL;
CREATE INDEX idx_workers_state ON workers (state, started_at DESC);
CREATE INDEX idx_workers_user ON workers (user_id);

---------------------------------------------------------------------
-- TASK RUNS (ADR 0016: autonomous execution)
---------------------------------------------------------------------
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

CREATE TABLE task_runs (
    run_id          UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    task_id         UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    attempt         INT  NOT NULL,
    parent_run_id   UUID REFERENCES task_runs(run_id),

    idempotency_key TEXT NOT NULL,

    state           run_state  NOT NULL DEFAULT 'scheduled',
    reason          run_reason NOT NULL DEFAULT 'ok',

    scheduled_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    claimed_at      TIMESTAMPTZ,
    started_at      TIMESTAMPTZ,
    ended_at        TIMESTAMPTZ,
    heartbeat_at    TIMESTAMPTZ,
    heartbeat_ttl_s INT NOT NULL DEFAULT 300,

    agent_id        UUID REFERENCES agents(agent_id),
    worker_id       UUID,
    session_id      UUID REFERENCES sessions(session_id),

    max_attempts    INT NOT NULL DEFAULT 3,
    retry_strategy  JSONB NOT NULL DEFAULT
        '{"kind":"exponential","base_ms":60000,"cap_ms":600000,"jitter":true}',
    deadline_at     TIMESTAMPTZ,

    input           JSONB NOT NULL DEFAULT '{}',
    output          JSONB,
    error           JSONB,

    output_commit_sha TEXT,
    output_branch     TEXT,
    output_pr_url     TEXT,
    output_worktree   TEXT,
    output_blob_ref   TEXT,

    fingerprint     TEXT,

    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (task_id, attempt),
    UNIQUE (idempotency_key),
    CHECK (attempt >= 1),
    CHECK (max_attempts >= 1)
);

CREATE INDEX idx_runs_ready ON task_runs (scheduled_at) WHERE state = 'ready';
CREATE INDEX idx_runs_live_heartbeat ON task_runs (heartbeat_at) WHERE state = 'running';
CREATE INDEX idx_runs_retry_candidates ON task_runs (ended_at) WHERE state IN ('failed', 'crashed');
CREATE INDEX idx_runs_deadline ON task_runs (deadline_at) WHERE state = 'running' AND deadline_at IS NOT NULL;
CREATE INDEX idx_runs_task ON task_runs (task_id, attempt DESC);
CREATE INDEX idx_runs_agent ON task_runs (agent_id, started_at DESC) WHERE agent_id IS NOT NULL;
CREATE INDEX idx_runs_worker ON task_runs (worker_id) WHERE worker_id IS NOT NULL;

-- Deferred FK: tasks.current_attempt_id -> task_runs.run_id
ALTER TABLE tasks ADD CONSTRAINT tasks_current_attempt_id_fkey
    FOREIGN KEY (current_attempt_id) REFERENCES task_runs(run_id);

-- task_events.run_id FK
ALTER TABLE task_events ADD CONSTRAINT task_events_run_id_fkey
    FOREIGN KEY (run_id) REFERENCES task_runs(run_id);
CREATE INDEX idx_task_events_run ON task_events (run_id) WHERE run_id IS NOT NULL;

---------------------------------------------------------------------
-- LEARNINGS
---------------------------------------------------------------------
CREATE TABLE learnings (
    learning_id    UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    repo_id        UUID REFERENCES repos(repo_id) ON DELETE CASCADE,
    file_glob      TEXT,
    rule_id        TEXT,
    text           TEXT NOT NULL,
    context        TEXT,
    created_by     UUID REFERENCES agents(agent_id),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    applied_count  INTEGER NOT NULL DEFAULT 0,
    scope_tags     JSONB NOT NULL DEFAULT '{}'::jsonb,
    user_id        TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_learnings_repo ON learnings (repo_id) WHERE repo_id IS NOT NULL;
CREATE INDEX idx_learnings_rule_id ON learnings (rule_id) WHERE rule_id IS NOT NULL;
CREATE INDEX idx_learnings_file_glob ON learnings (file_glob) WHERE file_glob IS NOT NULL;
CREATE INDEX idx_learnings_scope_tags ON learnings USING gin (scope_tags);

---------------------------------------------------------------------
-- BENCH TABLES
---------------------------------------------------------------------
CREATE TABLE bench_runs (
    run_id        UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    scenario      TEXT NOT NULL,
    baseline      TEXT NOT NULL,
    parallelism   INT  NOT NULL,
    model         TEXT NOT NULL,
    harness_sha   TEXT NOT NULL,
    seed          BIGINT,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at      TIMESTAMPTZ,
    passed        BOOLEAN,
    notes         TEXT,
    CHECK (parallelism >= 1)
);

CREATE INDEX idx_bench_runs_scenario ON bench_runs (scenario, baseline, started_at DESC);
CREATE INDEX idx_bench_runs_started ON bench_runs (started_at DESC);

CREATE TABLE bench_task_results (
    run_id        UUID NOT NULL REFERENCES bench_runs(run_id) ON DELETE CASCADE,
    task_idx      INT  NOT NULL,
    passed        BOOLEAN NOT NULL,
    wall_clock_s  INT NOT NULL,
    tokens_in     BIGINT,
    tokens_out    BIGINT,
    tokens_cache  BIGINT,
    usd           NUMERIC,
    reopened      BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (run_id, task_idx)
);

CREATE TABLE bench_metrics (
    run_id     UUID NOT NULL REFERENCES bench_runs(run_id) ON DELETE CASCADE,
    metric     TEXT NOT NULL,
    value      DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (run_id, metric)
);
