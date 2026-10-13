-- Immutable reservations, not activation authority. Participant authentication,
-- readiness and source backup verification belong to the coordinator workflow.
CREATE TABLE public.knowledge_fleet_operations (
    operation_id UUID PRIMARY KEY,
    request_sha256 TEXT NOT NULL CHECK (request_sha256 ~ '^[0-9a-f]{64}$'),
    database_id UUID NOT NULL,
    source_generation BIGINT NOT NULL CHECK (source_generation > 0),
    corpus_id UUID NOT NULL,
    participants UUID[] NOT NULL CHECK (
        array_ndims(participants) = 1 AND cardinality(participants) BETWEEN 1 AND 1024
        AND array_position(participants, NULL) IS NULL
        AND NOT ('00000000-0000-0000-0000-000000000000'::uuid = ANY(participants))
    ),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (database_id, source_generation)
);
REVOKE ALL ON public.knowledge_fleet_operations FROM PUBLIC;
CREATE TRIGGER ygg_knowledge_fleet_operation_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_fleet_operations FOR EACH STATEMENT
    EXECUTE FUNCTION public.ygg_knowledge_migration_event_guard();
