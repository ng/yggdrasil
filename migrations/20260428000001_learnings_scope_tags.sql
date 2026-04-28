-- Scoped learnings (yggdrasil-1): add scope_tags JSONB so learnings can be
-- filtered by agent identity and/or task kind at injection time.
-- Empty object = global (backward compat with existing rows).

ALTER TABLE learnings ADD COLUMN scope_tags JSONB NOT NULL DEFAULT '{}'::jsonb;

CREATE INDEX idx_learnings_scope_tags ON learnings USING gin (scope_tags);
