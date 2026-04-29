-- Clear embedding cache after model swap (all-minilm → qwen3-embedding).
-- Vectors from different models are not comparable; stale entries would
-- pollute similarity search until naturally evicted.
DELETE FROM embedding_cache WHERE model = 'all-minilm';
