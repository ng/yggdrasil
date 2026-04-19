-- Scoped learnings — CodeRabbit-style rule capture. Deterministic match
-- by (repo, file_glob, rule_id) rather than semantic similarity. This is
-- the orchestration-layer replacement for fuzzy-similarity retrieval of
-- durable rules (ADR 0015).

CREATE TABLE IF NOT EXISTS learnings (
    learning_id    UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    repo_id        UUID REFERENCES repos(repo_id) ON DELETE CASCADE,
    -- Glob pattern to match file paths against (e.g. `terraform/*.tf`,
    -- `src/**/*.rs`). NULL means "any file in the scope".
    file_glob      TEXT,
    -- External rule identifier (e.g. `CKV_AWS_337`, `clippy::too_many_lines`).
    -- Matches are deterministic — two learnings with the same rule_id in
    -- the same repo are treated as duplicates.
    rule_id        TEXT,
    -- The learning itself. Free text.
    text           TEXT NOT NULL,
    -- Where the learning came from (PR link, commit, quoted user turn).
    context        TEXT,
    created_by     UUID REFERENCES agents(agent_id),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Incremented each time the learning is surfaced. Cheap signal for
    -- "which rules actually fire" without building a separate events row.
    applied_count  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_learnings_repo ON learnings (repo_id) WHERE repo_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_learnings_rule_id ON learnings (rule_id) WHERE rule_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_learnings_file_glob ON learnings (file_glob) WHERE file_glob IS NOT NULL;
