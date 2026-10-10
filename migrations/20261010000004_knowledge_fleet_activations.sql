-- Durable authority for subsequent host finalization. The coordinator must still
-- authenticate fresh readiness and verify backups/publication before committing
-- the marker and this receipt in one transaction. No CLI activates fleets yet.
CREATE TABLE public.knowledge_fleet_activations (
    operation_id UUID PRIMARY KEY REFERENCES public.knowledge_fleet_operations(operation_id),
    generation BIGINT NOT NULL CHECK (generation > 0),
    prepared_sha256 TEXT NOT NULL CHECK (prepared_sha256 ~ '^[0-9a-f]{64}$'),
    backup_sha256 TEXT NOT NULL CHECK (backup_sha256 ~ '^[0-9a-f]{64}$'),
    ready_sha256 TEXT NOT NULL CHECK (ready_sha256 ~ '^[0-9a-f]{64}$'),
    ready_json TEXT NOT NULL CHECK (octet_length(ready_json) BETWEEN 1 AND 67108864),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK (encode(sha256(convert_to(ready_json, 'UTF8')), 'hex') = ready_sha256)
);
REVOKE ALL ON public.knowledge_fleet_activations FROM PUBLIC;
CREATE TRIGGER ygg_knowledge_fleet_activation_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_fleet_activations FOR EACH STATEMENT
    EXECUTE FUNCTION public.ygg_knowledge_migration_event_guard();

CREATE FUNCTION public.ygg_knowledge_fleet_activation_check()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER SET search_path=pg_catalog,pg_temp AS $$
DECLARE
    operation public.knowledge_fleet_operations%ROWTYPE;
    marker public.knowledge_storage%ROWTYPE;
    evidence JSONB;
    publication JSONB;
    hosts UUID[];
BEGIN
    -- The marker update uses the same advisory-before-row ordering.
    PERFORM pg_advisory_xact_lock(1497843531,1);
    SELECT * INTO STRICT operation FROM public.knowledge_fleet_operations WHERE operation_id=NEW.operation_id;
    SELECT * INTO STRICT marker FROM public.knowledge_storage WHERE singleton FOR UPDATE;
    IF NEW.generation IS DISTINCT FROM operation.source_generation + 2
        OR marker.generation IS DISTINCT FROM NEW.generation
        OR marker.database_id IS DISTINCT FROM operation.database_id
        OR marker.corpus_id IS DISTINCT FROM operation.corpus_id
        OR marker.backend IS DISTINCT FROM 'okf' THEN
        RAISE EXCEPTION 'fleet activation requires matching active marker' USING ERRCODE='55000';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM public.knowledge_fleet_events
        WHERE operation_id=NEW.operation_id AND step='fenced'
            AND prepared_sha256=NEW.prepared_sha256 AND backup_sha256=NEW.backup_sha256)
        OR EXISTS (SELECT 1 FROM public.knowledge_fleet_events WHERE operation_id=NEW.operation_id AND step='aborted')
        OR EXISTS (SELECT 1 FROM public.knowledge_migration_cancellations WHERE operation_id=NEW.operation_id) THEN
        RAISE EXCEPTION 'fleet activation requires matching un-aborted fence' USING ERRCODE='55000';
    END IF;
    evidence := NEW.ready_json::jsonb;
    publication := evidence->'publication';
    IF evidence->>'version' IS DISTINCT FROM '1'
        OR publication->>'version' IS DISTINCT FROM '1'
        OR publication->>'operation' IS DISTINCT FROM NEW.operation_id::text
        OR publication->>'request_sha256' IS DISTINCT FROM operation.request_sha256
        OR NOT coalesce(publication->>'commit' ~ '^([0-9a-f]{40}|[0-9a-f]{64})$',false)
        OR NOT coalesce(publication->>'manifest_sha256' ~ '^[0-9a-f]{64}$',false)
        OR NOT coalesce(publication->>'desired_sha256' ~ '^[0-9a-f]{64}$',false)
        OR jsonb_typeof(evidence->'participants') IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'fleet activation publication evidence differs' USING ERRCODE='55000';
    END IF;
    SELECT array_agg((host#>>'{readiness,preparation,coordinator,participant}')::uuid
        ORDER BY (host#>>'{readiness,preparation,coordinator,participant}')::uuid)
        INTO hosts FROM jsonb_array_elements(evidence->'participants') AS host;
    IF hosts IS DISTINCT FROM operation.participants THEN
        RAISE EXCEPTION 'fleet activation participant census differs' USING ERRCODE='55000';
    END IF;
    IF EXISTS (SELECT 1 FROM jsonb_array_elements(evidence->'participants') AS host WHERE
        host->>'version' IS DISTINCT FROM '1'
        OR host->>'operation' IS DISTINCT FROM NEW.operation_id::text
        OR host->>'request_sha256' IS DISTINCT FROM operation.request_sha256
        OR host#>'{readiness,publication}' IS DISTINCT FROM publication
        OR host#>>'{readiness,preparation,coordinator,migration_operation}' IS DISTINCT FROM NEW.operation_id::text
        OR host#>>'{readiness,preparation,database_id}' IS DISTINCT FROM operation.database_id::text
        OR host#>>'{readiness,preparation,corpus_id}' IS DISTINCT FROM operation.corpus_id::text
        OR host#>>'{readiness,preparation,source_generation}' IS DISTINCT FROM operation.source_generation::text
        OR NOT coalesce(host#>>'{readiness,swap_sha256}' ~ '^[0-9a-f]{64}$',false)
        OR NOT coalesce(host#>>'{readiness,archive_revision}' ~ '^[0-9a-f]{64}$',false)
        OR NOT coalesce(host#>>'{readiness,intent_sha256}' ~ '^[0-9a-f]{64}$',false)) THEN
        RAISE EXCEPTION 'fleet activation host evidence differs' USING ERRCODE='55000';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER ygg_knowledge_fleet_activation_check BEFORE INSERT
    ON public.knowledge_fleet_activations FOR EACH ROW
    EXECUTE FUNCTION public.ygg_knowledge_fleet_activation_check();
