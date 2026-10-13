-- All authenticated restoration acknowledgements must be sealed before a fresh
-- rollback can reserve the same still-active OKF generation.
CREATE TABLE public.knowledge_fleet_rollback_completions (
    operation_id UUID PRIMARY KEY REFERENCES public.knowledge_fleet_rollback_cancellations(operation_id),
    hosts_sha256 TEXT NOT NULL CHECK (hosts_sha256 ~ '^[0-9a-f]{64}$'),
    hosts_json TEXT NOT NULL CHECK (octet_length(hosts_json) BETWEEN 1 AND 67108864),
    CHECK (hosts_sha256=encode(sha256(convert_to(hosts_json,'UTF8')),'hex')),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
REVOKE ALL ON public.knowledge_fleet_rollback_completions FROM PUBLIC;
CREATE TRIGGER ygg_knowledge_fleet_rollback_completion_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_fleet_rollback_completions FOR EACH STATEMENT
    EXECUTE FUNCTION public.ygg_knowledge_migration_event_guard();
CREATE FUNCTION public.ygg_knowledge_fleet_rollback_completion_check()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER SET search_path=pg_catalog,pg_temp AS $$
DECLARE
    reverse_op public.knowledge_fleet_rollbacks%ROWTYPE;
    forward_op public.knowledge_fleet_operations%ROWTYPE;
    marker public.knowledge_storage%ROWTYPE;
    evidence JSONB;
    hosts UUID[];
BEGIN
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'rollback completion requires READ COMMITTED' USING ERRCODE='55000';
    END IF;
    PERFORM pg_advisory_xact_lock(1497843531,1);
    SELECT * INTO STRICT reverse_op FROM public.knowledge_fleet_rollbacks WHERE operation_id=NEW.operation_id;
    SELECT * INTO STRICT forward_op FROM public.knowledge_fleet_operations WHERE operation_id=reverse_op.forward_operation_id;
    SELECT * INTO STRICT marker FROM public.knowledge_storage WHERE singleton FOR UPDATE;
    IF marker.database_id IS DISTINCT FROM reverse_op.database_id
        OR marker.generation IS DISTINCT FROM reverse_op.source_generation
        OR marker.backend IS DISTINCT FROM 'okf'
        OR marker.corpus_id IS DISTINCT FROM forward_op.corpus_id
        OR EXISTS (SELECT 1 FROM public.knowledge_fleet_rollback_fences WHERE operation_id=NEW.operation_id)
        OR NOT EXISTS (SELECT 1 FROM public.knowledge_fleet_rollback_cancellations c WHERE c.operation_id=NEW.operation_id
            AND c.request_sha256=reverse_op.request_sha256 AND c.database_id=reverse_op.database_id AND c.source_generation=reverse_op.source_generation) THEN
        RAISE EXCEPTION 'rollback completion requires cancelled original OKF generation' USING ERRCODE='55000';
    END IF;
    evidence := NEW.hosts_json::jsonb;
    IF evidence->>'version' IS DISTINCT FROM '1'
        OR evidence->>'rollback_sha256' IS DISTINCT FROM reverse_op.request_sha256
        OR jsonb_typeof(evidence->'participants') IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'rollback completion request evidence differs' USING ERRCODE='55000';
    END IF;
    SELECT array_agg((host#>>'{coordinator,participant}')::uuid ORDER BY (host#>>'{coordinator,participant}')::uuid)
        INTO hosts FROM jsonb_array_elements(evidence->'participants') AS host;
    IF hosts IS DISTINCT FROM forward_op.participants THEN
        RAISE EXCEPTION 'rollback completion participant census differs' USING ERRCODE='55000';
    END IF;
    IF EXISTS (SELECT 1 FROM jsonb_array_elements(evidence->'participants') AS host WHERE
        host#>>'{coordinator,migration_operation}' IS DISTINCT FROM NEW.operation_id::text
        OR host->>'request_sha256' IS DISTINCT FROM reverse_op.request_sha256
        OR host->>'database_id' IS DISTINCT FROM reverse_op.database_id::text
        OR host->>'corpus_id' IS DISTINCT FROM forward_op.corpus_id::text
        OR host->>'source_generation' IS DISTINCT FROM reverse_op.source_generation::text
        OR NOT coalesce(host->>'original_sha256' ~ '^[0-9a-f]{64}$', false)
        OR jsonb_typeof(host->'policy') IS DISTINCT FROM 'string'
        OR NOT (host ? 'fence')
        OR (host->'fence' <> 'null'::jsonb AND (
            jsonb_typeof(host->'fence') IS DISTINCT FROM 'object'
            OR host#>'{fence,coordinator}' IS DISTINCT FROM host->'coordinator'
            OR host#>>'{fence,database_id}' IS DISTINCT FROM host->>'database_id'
            OR host#>>'{fence,corpus_id}' IS DISTINCT FROM host->>'corpus_id'
            OR host#>>'{fence,source_generation}' IS DISTINCT FROM host->>'source_generation'
            OR host#>>'{fence,policy}' IS DISTINCT FROM host->>'policy'
            OR host#>>'{fence,original_sha256}' IS DISTINCT FROM host->>'original_sha256'
            OR NOT coalesce(host#>>'{fence,fenced_sha256}' ~ '^[0-9a-f]{64}$', false)))) THEN
        RAISE EXCEPTION 'rollback restoration evidence differs' USING ERRCODE='55000';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER ygg_knowledge_fleet_rollback_completion_check BEFORE INSERT
    ON public.knowledge_fleet_rollback_completions FOR EACH ROW
    EXECUTE FUNCTION public.ygg_knowledge_fleet_rollback_completion_check();

-- Preserve every historical request. The generation lease plus current snapshots
-- enforce one unfinished reservation while permitting fully-cancelled successors.
ALTER TABLE public.knowledge_fleet_rollbacks DROP CONSTRAINT knowledge_fleet_rollbacks_database_id_source_generation_key;
CREATE INDEX knowledge_fleet_rollback_generation ON public.knowledge_fleet_rollbacks(database_id,source_generation);
CREATE FUNCTION public.ygg_knowledge_fleet_rollback_admission_check()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER SET search_path=pg_catalog,pg_temp AS $$
DECLARE marker public.knowledge_storage%ROWTYPE;
BEGIN
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'rollback admission requires READ COMMITTED' USING ERRCODE='55000';
    END IF;
    PERFORM pg_advisory_xact_lock(1497843531,1);
    SELECT * INTO STRICT marker FROM public.knowledge_storage WHERE singleton FOR UPDATE;
    IF marker.database_id IS DISTINCT FROM NEW.database_id OR marker.generation IS DISTINCT FROM NEW.source_generation OR marker.backend IS DISTINCT FROM 'okf' THEN
        RAISE EXCEPTION 'rollback admission requires active OKF generation' USING ERRCODE='55000';
    END IF;
    IF EXISTS (SELECT 1 FROM public.knowledge_fleet_rollbacks r WHERE r.database_id=NEW.database_id AND r.source_generation=NEW.source_generation
        AND NOT EXISTS (SELECT 1 FROM public.knowledge_fleet_rollback_completions c WHERE c.operation_id=r.operation_id)) THEN
        RAISE EXCEPTION 'another rollback operation owns this active generation' USING ERRCODE='55000';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER ygg_knowledge_fleet_rollback_admission_check BEFORE INSERT
    ON public.knowledge_fleet_rollbacks FOR EACH ROW
    EXECUTE FUNCTION public.ygg_knowledge_fleet_rollback_admission_check();
