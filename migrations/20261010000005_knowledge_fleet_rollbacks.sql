-- Reserve one reverse operation for the current activated fleet. This is not
-- evidence that hosts are fenced or that a reverse import has happened.
CREATE TABLE public.knowledge_fleet_rollbacks (
    operation_id UUID PRIMARY KEY CHECK (operation_id <> '00000000-0000-0000-0000-000000000000'::uuid),
    forward_operation_id UUID NOT NULL REFERENCES public.knowledge_fleet_activations(operation_id),
    database_id UUID NOT NULL,
    source_generation BIGINT NOT NULL CHECK (source_generation > 0 AND source_generation < 9223372036854775806),
    request_sha256 TEXT NOT NULL CHECK (request_sha256 ~ '^[0-9a-f]{64}$'),
    request_json TEXT NOT NULL CHECK (octet_length(request_json) BETWEEN 1 AND 1048576),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK (operation_id <> forward_operation_id),
    CHECK (request_sha256 = encode(sha256(convert_to(request_json, 'UTF8')), 'hex')),
    UNIQUE (database_id, source_generation)
);
REVOKE ALL ON public.knowledge_fleet_rollbacks FROM PUBLIC;
CREATE TRIGGER ygg_knowledge_fleet_rollback_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_fleet_rollbacks FOR EACH STATEMENT
    EXECUTE FUNCTION public.ygg_knowledge_migration_event_guard();
