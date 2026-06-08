-- ADR 0015 phases 2–4: retire the embedding / similarity-retrieval layer.
--
-- Yggdrasil keeps its orchestration substrate (tasks, locks, agents,
-- sessions, events, runs) and drops everything that existed only to power
-- vector retrieval: the nodes DAG ledger, scoped memories, the embedding
-- cache, and the tasks.embedding column (task dupe-detection now uses
-- string similarity, not embeddings).
--
-- nodes is referenced by agents.head_node_id / agents.digest_id and
-- sessions.head_node_id. DROP ... CASCADE removes those foreign-key
-- constraints automatically, but it does NOT rewrite the columns — they
-- would keep dangling UUIDs pointing at rows that no longer exist. Null
-- them first so the agent/session models see clean NULLs and the columns
-- survive as plain nullable UUIDs.

UPDATE agents
SET head_node_id = NULL,
    digest_id = NULL
WHERE head_node_id IS NOT NULL
   OR digest_id IS NOT NULL;

UPDATE sessions
SET head_node_id = NULL
WHERE head_node_id IS NOT NULL;

DROP TABLE IF EXISTS nodes CASCADE;
DROP TABLE IF EXISTS memories CASCADE;
DROP TABLE IF EXISTS embedding_cache CASCADE;

ALTER TABLE tasks DROP COLUMN IF EXISTS embedding;

-- The nodes content_tsv trigger function is orphaned once nodes is gone.
DROP FUNCTION IF EXISTS nodes_content_tsv_update() CASCADE;

-- No vector columns remain anywhere, so the pgvector extension is no
-- longer needed.
DROP EXTENSION IF EXISTS vector;
