-- Embedding cache: sha256(text) → vector memoization.
--
-- Same text hashed under the same model always yields the same vector, so
-- this is a pure content-addressable cache. Keyed on (content_hash, model)
-- to stay correct across model swaps. Vector dimension is fixed to 384
-- (matches the rest of the schema). A different-dim model would need a
-- parallel cache table rather than a mixed one.
--
-- `hit_count` + `last_hit_at` are used by the cache-observability surface;
-- they also enable a future LRU eviction policy without schema changes.

CREATE TABLE embedding_cache (
    content_hash  BYTEA NOT NULL,
    model         TEXT NOT NULL,
    embedding     vector(384) NOT NULL,
    hit_count     BIGINT NOT NULL DEFAULT 0,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_hit_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (content_hash, model)
);

CREATE INDEX idx_embedding_cache_last_hit ON embedding_cache (last_hit_at DESC);
