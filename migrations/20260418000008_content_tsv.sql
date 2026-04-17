-- Hybrid retrieval: add a generated tsvector column over the text-like
-- fields of nodes.content so we can UNION pgvector and Postgres full-text
-- candidates in a single query. Jsonb_path_ops would be overkill; we only
-- need text from the string leaves we actually store.

ALTER TABLE nodes ADD COLUMN content_tsv tsvector
    GENERATED ALWAYS AS (
        to_tsvector(
            'english',
            COALESCE(content->>'text',      '') || ' ' ||
            COALESCE(content->>'directive', '') || ' ' ||
            COALESCE(content->>'summary',   '') || ' ' ||
            COALESCE(content->>'feedback',  '')
        )
    ) STORED;

CREATE INDEX idx_nodes_content_tsv ON nodes USING GIN (content_tsv);
