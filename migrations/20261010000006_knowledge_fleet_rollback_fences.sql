-- Exact authenticated host evidence committed atomically with the reverse fence.
CREATE TABLE public.knowledge_fleet_rollback_fences (
    operation_id UUID PRIMARY KEY REFERENCES public.knowledge_fleet_rollbacks(operation_id),
    generation BIGINT NOT NULL,
    hosts_sha256 TEXT NOT NULL CHECK (hosts_sha256 ~ '^[0-9a-f]{64}$'),
    hosts_json TEXT NOT NULL CHECK (octet_length(hosts_json) BETWEEN 1 AND 67108864),
    remote_commit TEXT NOT NULL CHECK (remote_commit ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    CHECK (hosts_sha256=encode(sha256(convert_to(hosts_json,'UTF8')),'hex')),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
REVOKE ALL ON public.knowledge_fleet_rollback_fences FROM PUBLIC;
CREATE TRIGGER ygg_knowledge_fleet_rollback_fence_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_fleet_rollback_fences FOR EACH STATEMENT
    EXECUTE FUNCTION public.ygg_knowledge_migration_event_guard();
CREATE FUNCTION public.ygg_knowledge_fleet_rollback_fence_check()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER SET search_path=pg_catalog,pg_temp AS $$
DECLARE
    reverse_op public.knowledge_fleet_rollbacks%ROWTYPE;
    forward_op public.knowledge_fleet_operations%ROWTYPE;
    marker public.knowledge_storage%ROWTYPE;
    evidence JSONB;
    hosts UUID[];
BEGIN
    PERFORM pg_advisory_xact_lock(1497843531,1);
    SELECT * INTO STRICT reverse_op FROM public.knowledge_fleet_rollbacks WHERE operation_id=NEW.operation_id;
    SELECT * INTO STRICT forward_op FROM public.knowledge_fleet_operations WHERE operation_id=reverse_op.forward_operation_id;
    SELECT * INTO STRICT marker FROM public.knowledge_storage WHERE singleton FOR UPDATE;
    IF NEW.generation IS DISTINCT FROM reverse_op.source_generation+1
        OR marker.generation IS DISTINCT FROM NEW.generation
        OR marker.database_id IS DISTINCT FROM reverse_op.database_id
        OR marker.corpus_id IS DISTINCT FROM forward_op.corpus_id
        OR marker.backend IS DISTINCT FROM 'fenced'
        OR NEW.remote_commit IS DISTINCT FROM reverse_op.request_json::jsonb->>'expected_remote_commit' THEN
        RAISE EXCEPTION 'rollback fence requires matching generation and remote' USING ERRCODE='55000';
    END IF;
    evidence := NEW.hosts_json::jsonb;
    IF evidence->>'version' IS DISTINCT FROM '1'
        OR evidence->>'rollback_sha256' IS DISTINCT FROM reverse_op.request_sha256
        OR jsonb_typeof(evidence->'participants') IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'rollback fence request evidence differs' USING ERRCODE='55000';
    END IF;
    SELECT array_agg((host#>>'{coordinator,participant}')::uuid ORDER BY (host#>>'{coordinator,participant}')::uuid)
        INTO hosts FROM jsonb_array_elements(evidence->'participants') AS host;
    IF hosts IS DISTINCT FROM forward_op.participants THEN
        RAISE EXCEPTION 'rollback fence participant census differs' USING ERRCODE='55000';
    END IF;
    IF EXISTS (SELECT 1 FROM jsonb_array_elements(evidence->'participants') AS host WHERE
        host#>>'{coordinator,migration_operation}' IS DISTINCT FROM NEW.operation_id::text
        OR host->>'database_id' IS DISTINCT FROM reverse_op.database_id::text
        OR host->>'corpus_id' IS DISTINCT FROM forward_op.corpus_id::text
        OR host->>'source_generation' IS DISTINCT FROM reverse_op.source_generation::text
        OR NOT coalesce(host->>'original_sha256' ~ '^[0-9a-f]{64}$',false)
        OR NOT coalesce(host->>'fenced_sha256' ~ '^[0-9a-f]{64}$',false)) THEN
        RAISE EXCEPTION 'rollback host fence differs' USING ERRCODE='55000';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER ygg_knowledge_fleet_rollback_fence_check BEFORE INSERT
    ON public.knowledge_fleet_rollback_fences FOR EACH ROW
    EXECUTE FUNCTION public.ygg_knowledge_fleet_rollback_fence_check();
