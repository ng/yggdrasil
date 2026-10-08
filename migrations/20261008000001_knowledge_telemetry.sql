-- Optional operational usage, independent of authoritative OKF content. Imported
-- baselines and post-cutover observations are separate so migration retries do
-- not reset or double-count applications already recorded here.
CREATE TABLE knowledge_usage (
    corpus_id UUID NOT NULL,
    document_id UUID NOT NULL,
    imported_count INTEGER,
    imported_last_applied_at TIMESTAMPTZ,
    observed_count BIGINT NOT NULL DEFAULT 0 CHECK (observed_count >= 0),
    observed_last_applied_at TIMESTAMPTZ,
    PRIMARY KEY (corpus_id, document_id),
    CHECK (imported_count IS NOT NULL OR imported_last_applied_at IS NULL)
);

-- Retain application IDs to make retries after ambiguous commits idempotent.
-- This is separate from per-session injection deduplication.
CREATE TABLE knowledge_applications (
    corpus_id UUID NOT NULL,
    document_id UUID NOT NULL,
    application_id UUID NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (corpus_id, document_id, application_id),
    FOREIGN KEY (corpus_id, document_id)
        REFERENCES knowledge_usage (corpus_id, document_id) ON DELETE CASCADE
);
