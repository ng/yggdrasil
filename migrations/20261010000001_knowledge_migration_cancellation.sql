-- Cancellation before a knowledge fence has no source backup to bind. Retain
-- its own immutable intent receipt and advance SQL generation atomically so even
-- older coordinators cannot fence using the cancelled source generation.
CREATE TABLE public.knowledge_migration_cancellations (
    operation_id UUID PRIMARY KEY,
    request_sha256 TEXT NOT NULL CHECK (request_sha256 ~ '^[0-9a-f]{64}$'),
    database_id UUID NOT NULL,
    source_generation BIGINT NOT NULL CHECK (source_generation > 0),
    target_generation BIGINT NOT NULL CHECK (target_generation = source_generation + 1),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (database_id, target_generation)
);
REVOKE ALL ON public.knowledge_migration_cancellations FROM PUBLIC;
CREATE TRIGGER ygg_knowledge_cancellation_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_migration_cancellations FOR EACH STATEMENT
    EXECUTE FUNCTION public.ygg_knowledge_migration_event_guard();
