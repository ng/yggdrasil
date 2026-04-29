-- Switch to embeddinggemma (768d native). Hard reset: drop and recreate
-- vector columns since old 384d vectors are incompatible.

-- nodes
ALTER TABLE nodes DROP COLUMN IF EXISTS embedding;
ALTER TABLE nodes ADD COLUMN embedding vector(768);
CREATE INDEX IF NOT EXISTS nodes_embedding_idx ON nodes USING hnsw (embedding vector_cosine_ops);

-- embedding_cache: full wipe — model changed and dimensionality changed
TRUNCATE embedding_cache;
ALTER TABLE embedding_cache DROP COLUMN IF EXISTS embedding;
ALTER TABLE embedding_cache ADD COLUMN embedding vector(768) NOT NULL;

-- tasks
ALTER TABLE tasks DROP COLUMN IF EXISTS embedding;
ALTER TABLE tasks ADD COLUMN embedding vector(768);

-- memories
ALTER TABLE memories DROP COLUMN IF EXISTS embedding;
ALTER TABLE memories ADD COLUMN embedding vector(768);
