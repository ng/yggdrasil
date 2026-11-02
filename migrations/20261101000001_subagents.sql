-- Native Claude Code subagents (ADR 0020). One row per Agent tool call,
-- written by `ygg hook pre-tool-use` (state 'pending', carries the
-- description), claimed by `ygg hook subagent-start` (state 'running', gains
-- the Claude Code agent id) and closed by `ygg hook subagent-stop` (state
-- 'done', token totals from the subagent's own transcript). Subagents are
-- children of the parent agent, not rows in `agents`: they are per-call and
-- short-lived.
CREATE TABLE subagents (
    subagent_id      UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    parent_agent_id  UUID NOT NULL REFERENCES agents(agent_id) ON DELETE CASCADE,
    user_id          TEXT NOT NULL DEFAULT '',
    cc_session_id    TEXT NOT NULL,
    cc_agent_id      TEXT,                   -- NULL while pending
    tool_use_id      TEXT,                   -- the parent's Agent tool call, when seen
    agent_type       TEXT NOT NULL DEFAULT '',
    description      TEXT,
    background       BOOLEAN NOT NULL DEFAULT false,
    isolation        TEXT,
    state            TEXT NOT NULL DEFAULT 'pending'
                     CHECK (state IN ('pending', 'running', 'done')),
    cwd              TEXT,
    last_tool        TEXT,
    transcript_path  TEXT,
    last_message     TEXT,
    input_tokens     BIGINT NOT NULL DEFAULT 0,
    output_tokens    BIGINT NOT NULL DEFAULT 0,
    cache_read       BIGINT NOT NULL DEFAULT 0,
    cache_write      BIGINT NOT NULL DEFAULT 0,
    tool_calls       INT NOT NULL DEFAULT 0,
    started_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at         TIMESTAMPTZ
);

-- Hook idempotency: a repeated SubagentStart/Stop for the same subagent, or a
-- retried PreToolUse for the same Agent call, hits these keys.
CREATE UNIQUE INDEX subagents_session_agent_uk
    ON subagents (cc_session_id, cc_agent_id) WHERE cc_agent_id IS NOT NULL;
CREATE UNIQUE INDEX subagents_session_tool_use_uk
    ON subagents (cc_session_id, tool_use_id) WHERE tool_use_id IS NOT NULL;
CREATE INDEX idx_subagents_parent ON subagents (parent_agent_id, state, updated_at DESC);
CREATE INDEX idx_subagents_recent ON subagents (updated_at DESC) WHERE state <> 'pending';
