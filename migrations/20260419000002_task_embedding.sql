-- Task embedding for duplicate detection. Title + short description
-- concatenated and embedded by the same all-minilm (384-dim) model the
-- rest of yggdrasil uses, so tasks share an index space with nodes/memories.

ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS embedding vector(384);

-- HNSW cosine index — mirrors migrations/20260418000012_memories.sql
-- (m=16, ef_construction=64). Small enough to rebuild cheaply.
CREATE INDEX IF NOT EXISTS idx_tasks_embedding
    ON tasks
    USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);
