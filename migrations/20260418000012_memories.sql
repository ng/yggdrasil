-- First-class memories table. Separate from nodes because memories have
-- explicit lifecycle (scope, pin, expire) and we want to retrieve them
-- without fishing through transcript nodes by kind=directive.

CREATE TYPE memory_scope AS ENUM ('global', 'repo', 'session');

CREATE TABLE memories (
    memory_id       UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    scope           memory_scope NOT NULL,
    repo_id         UUID REFERENCES repos(repo_id) ON DELETE CASCADE,
    cc_session_id   TEXT,
    agent_id        UUID,
    agent_name      TEXT NOT NULL DEFAULT '',
    text            TEXT NOT NULL,
    embedding       vector(384),
    pinned          BOOLEAN NOT NULL DEFAULT false,
    expires_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Scope discriminator: each scope requires its corresponding key.
    CONSTRAINT memory_scope_repo CHECK (scope <> 'repo' OR repo_id IS NOT NULL),
    CONSTRAINT memory_scope_session CHECK (scope <> 'session' OR cc_session_id IS NOT NULL)
);

CREATE INDEX idx_memories_scope    ON memories (scope, created_at DESC);
CREATE INDEX idx_memories_repo     ON memories (repo_id)       WHERE scope = 'repo';
CREATE INDEX idx_memories_session  ON memories (cc_session_id) WHERE scope = 'session';
CREATE INDEX idx_memories_pinned   ON memories (pinned)        WHERE pinned = true;
CREATE INDEX idx_memories_expires  ON memories (expires_at)    WHERE expires_at IS NOT NULL;
CREATE INDEX idx_memories_embedding ON memories
    USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);
