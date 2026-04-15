CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

---------------------------------------------------------------------
-- NODES: The DAG ledger
---------------------------------------------------------------------
CREATE TYPE node_kind AS ENUM (
    'user_message',
    'assistant_message',
    'tool_call',
    'tool_result',
    'digest',
    'directive',
    'system',
    'human_override'
);

CREATE TABLE nodes (
    id          UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    parent_id   UUID REFERENCES nodes(id),
    agent_id    UUID NOT NULL,
    kind        node_kind NOT NULL,
    content     JSONB NOT NULL,
    token_count INT NOT NULL DEFAULT 0,
    embedding   vector(384),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    ancestors   UUID[] NOT NULL DEFAULT '{}'
);

CREATE INDEX idx_nodes_parent ON nodes (parent_id);
CREATE INDEX idx_nodes_agent ON nodes (agent_id, created_at DESC);
CREATE INDEX idx_nodes_ancestors ON nodes USING GIN (ancestors);
CREATE INDEX idx_nodes_embedding ON nodes
    USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);

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
    CONSTRAINT uq_lock_resource UNIQUE (resource_key)
);

CREATE INDEX idx_locks_agent ON locks (agent_id);
CREATE INDEX idx_locks_expiry ON locks (expires_at);

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
    agent_name      TEXT NOT NULL UNIQUE,
    current_state   agent_state NOT NULL DEFAULT 'idle',
    head_node_id    UUID REFERENCES nodes(id),
    digest_id       UUID REFERENCES nodes(id),
    context_tokens  INT NOT NULL DEFAULT 0,
    metadata        JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

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
