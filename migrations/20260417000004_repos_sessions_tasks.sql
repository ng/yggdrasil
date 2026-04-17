---------------------------------------------------------------------
-- REPOS: First-class repository identity
---------------------------------------------------------------------
CREATE TABLE repos (
    repo_id       UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    canonical_url TEXT UNIQUE,                            -- git remote.origin.url; NULL for non-git
    name          TEXT NOT NULL,                          -- display name
    task_prefix   TEXT NOT NULL UNIQUE,                   -- used for task IDs: "<prefix>-NNN"
    local_paths   TEXT[] NOT NULL DEFAULT '{}',           -- where this repo has been seen on this host
    metadata      JSONB NOT NULL DEFAULT '{}',
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_repos_prefix ON repos (task_prefix);

---------------------------------------------------------------------
-- SESSIONS: One row per Claude Code session (agent × repo × time)
---------------------------------------------------------------------
CREATE TABLE sessions (
    session_id  UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    agent_id    UUID NOT NULL REFERENCES agents(agent_id),
    repo_id     UUID REFERENCES repos(repo_id),
    started_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at    TIMESTAMPTZ,
    metadata    JSONB NOT NULL DEFAULT '{}'
);

CREATE INDEX idx_sessions_agent ON sessions (agent_id, started_at DESC);
CREATE INDEX idx_sessions_repo  ON sessions (repo_id, started_at DESC);

---------------------------------------------------------------------
-- NODES: add optional session + repo linkage (backwards compatible)
---------------------------------------------------------------------
ALTER TABLE nodes ADD COLUMN session_id UUID REFERENCES sessions(session_id);
ALTER TABLE nodes ADD COLUMN repo_id    UUID REFERENCES repos(repo_id);

CREATE INDEX idx_nodes_session ON nodes (session_id, created_at DESC);
CREATE INDEX idx_nodes_repo    ON nodes (repo_id, created_at DESC);

---------------------------------------------------------------------
-- TASKS: beads-style issue tracking scoped to repos
---------------------------------------------------------------------
CREATE TYPE task_status AS ENUM ('open', 'in_progress', 'blocked', 'closed');
CREATE TYPE task_kind   AS ENUM ('task', 'bug', 'feature', 'chore', 'epic');

CREATE TABLE tasks (
    task_id       UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    repo_id       UUID NOT NULL REFERENCES repos(repo_id) ON DELETE CASCADE,
    seq           INT NOT NULL,                              -- per-repo sequence
    title         TEXT NOT NULL,
    description   TEXT NOT NULL DEFAULT '',
    acceptance    TEXT,
    design        TEXT,
    notes         TEXT,
    kind          task_kind NOT NULL DEFAULT 'task',
    status        task_status NOT NULL DEFAULT 'open',
    priority      SMALLINT NOT NULL DEFAULT 2 CHECK (priority BETWEEN 0 AND 4),
    created_by    UUID REFERENCES agents(agent_id),
    assignee      UUID REFERENCES agents(agent_id),
    human_flag    BOOLEAN NOT NULL DEFAULT FALSE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at     TIMESTAMPTZ,
    close_reason  TEXT,
    UNIQUE (repo_id, seq)
);

CREATE INDEX idx_tasks_repo_status ON tasks (repo_id, status, priority);
CREATE INDEX idx_tasks_assignee    ON tasks (assignee) WHERE assignee IS NOT NULL;

-- Task dependencies: task depends on blocker
CREATE TABLE task_deps (
    task_id     UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    blocker_id  UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, blocker_id),
    CHECK (task_id <> blocker_id)
);

CREATE INDEX idx_task_deps_blocker ON task_deps (blocker_id);

-- Labels (many-to-many, simple string)
CREATE TABLE task_labels (
    task_id  UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    label    TEXT NOT NULL,
    PRIMARY KEY (task_id, label)
);

CREATE INDEX idx_task_labels_label ON task_labels (label);

-- Per-task audit trail
CREATE TABLE task_events (
    event_id    UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    task_id     UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    agent_id    UUID REFERENCES agents(agent_id),
    kind        TEXT NOT NULL,
    payload     JSONB NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_task_events_task ON task_events (task_id, created_at DESC);

-- Per-repo sequence counter for assigning task seq numbers
CREATE TABLE task_seq (
    repo_id   UUID PRIMARY KEY REFERENCES repos(repo_id) ON DELETE CASCADE,
    next_seq  INT NOT NULL DEFAULT 1
);
