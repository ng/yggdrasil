-- content_tsv compat: GENERATED STORED with to_tsvector works on PG12+ but
-- some install configs (older pg_trgm, non-standard text configs) choke on
-- the "immutability" check. Rewriting as a trigger-maintained column works
-- from PG 9.6 onward and sidesteps the issue entirely.

-- Drop the dependent index + generated column.
DROP INDEX IF EXISTS idx_nodes_content_tsv;
ALTER TABLE nodes DROP COLUMN IF EXISTS content_tsv;

-- Plain column, filled by trigger on insert/update.
ALTER TABLE nodes ADD COLUMN content_tsv tsvector;

CREATE OR REPLACE FUNCTION nodes_content_tsv_update() RETURNS trigger AS $$
BEGIN
    NEW.content_tsv := to_tsvector(
        'english',
        COALESCE(NEW.content->>'text',      '') || ' ' ||
        COALESCE(NEW.content->>'directive', '') || ' ' ||
        COALESCE(NEW.content->>'summary',   '') || ' ' ||
        COALESCE(NEW.content->>'feedback',  '')
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_nodes_content_tsv ON nodes;
CREATE TRIGGER trg_nodes_content_tsv
BEFORE INSERT OR UPDATE OF content ON nodes
FOR EACH ROW EXECUTE FUNCTION nodes_content_tsv_update();

-- Backfill existing rows.
UPDATE nodes SET content_tsv = to_tsvector(
    'english',
    COALESCE(content->>'text',      '') || ' ' ||
    COALESCE(content->>'directive', '') || ' ' ||
    COALESCE(content->>'summary',   '') || ' ' ||
    COALESCE(content->>'feedback',  '')
);

CREATE INDEX idx_nodes_content_tsv ON nodes USING GIN (content_tsv);
