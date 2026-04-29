---------------------------------------------------------------------
-- Multi-user support: partition core tables by user identity.
--
-- user_id is resolved at runtime from YGG_USER env or `whoami`.
-- Existing rows keep user_id = '' (the default / legacy namespace).
-- Child tables (task_deps, task_labels, task_events, task_seq,
-- agent_stats, bench_*) inherit scoping via FK and need no column.
---------------------------------------------------------------------

-- Core identity tables
ALTER TABLE agents   ADD COLUMN IF NOT EXISTS user_id TEXT NOT NULL DEFAULT '';
ALTER TABLE repos    ADD COLUMN IF NOT EXISTS user_id TEXT NOT NULL DEFAULT '';

-- Data tables
ALTER TABLE tasks    ADD COLUMN IF NOT EXISTS user_id TEXT NOT NULL DEFAULT '';
ALTER TABLE locks    ADD COLUMN IF NOT EXISTS user_id TEXT NOT NULL DEFAULT '';
ALTER TABLE nodes    ADD COLUMN IF NOT EXISTS user_id TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS user_id TEXT NOT NULL DEFAULT '';
ALTER TABLE events   ADD COLUMN IF NOT EXISTS user_id TEXT NOT NULL DEFAULT '';
ALTER TABLE memories ADD COLUMN IF NOT EXISTS user_id TEXT NOT NULL DEFAULT '';
ALTER TABLE workers  ADD COLUMN IF NOT EXISTS user_id TEXT NOT NULL DEFAULT '';
ALTER TABLE learnings ADD COLUMN IF NOT EXISTS user_id TEXT NOT NULL DEFAULT '';

---------------------------------------------------------------------
-- Unique constraints: agent identity is now (user_id, name, persona)
---------------------------------------------------------------------
DROP INDEX IF EXISTS agents_name_persona_uk;
CREATE UNIQUE INDEX agents_name_persona_user_uk
    ON agents (user_id, agent_name, COALESCE(persona, ''));

-- Repos: (user_id, task_prefix) must be unique so two users can
-- register the same repo independently. Drop the old global unique.
ALTER TABLE repos DROP CONSTRAINT IF EXISTS repos_task_prefix_key;
CREATE UNIQUE INDEX repos_user_prefix_uk ON repos (user_id, task_prefix);

---------------------------------------------------------------------
-- Filtered indexes for the most common query patterns
---------------------------------------------------------------------
CREATE INDEX IF NOT EXISTS idx_agents_user   ON agents (user_id) WHERE archived_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_tasks_user    ON tasks (user_id, repo_id) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_locks_user    ON locks (user_id);
CREATE INDEX IF NOT EXISTS idx_nodes_user    ON nodes (user_id, agent_id);
CREATE INDEX IF NOT EXISTS idx_repos_user    ON repos (user_id);
CREATE INDEX IF NOT EXISTS idx_memories_user ON memories (user_id);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions (user_id, agent_id);
CREATE INDEX IF NOT EXISTS idx_events_user   ON events (user_id);
CREATE INDEX IF NOT EXISTS idx_workers_user  ON workers (user_id);
