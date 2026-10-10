-- A new explicit request selects a descendant snapshot without rewriting any
-- original request, fence, cache or backup. A predecessor has at most one child.
CREATE TABLE public.knowledge_fleet_rollback_reconciliations (
    operation_id UUID PRIMARY KEY,
    rollback_operation_id UUID NOT NULL REFERENCES public.knowledge_fleet_rollback_fences(operation_id),
    previous_request_sha256 TEXT NOT NULL CHECK (previous_request_sha256 ~ '^[0-9a-f]{64}$'),
    previous_remote_commit TEXT NOT NULL CHECK (previous_remote_commit ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    remote_commit TEXT NOT NULL CHECK (remote_commit ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    request_sha256 TEXT NOT NULL UNIQUE CHECK (request_sha256 ~ '^[0-9a-f]{64}$'),
    request_json TEXT NOT NULL CHECK (octet_length(request_json) BETWEEN 1 AND 1048576),
    hosts_sha256 TEXT NOT NULL CHECK (hosts_sha256 ~ '^[0-9a-f]{64}$'),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK (request_sha256=encode(sha256(convert_to(request_json,'UTF8')),'hex')),
    CHECK (remote_commit <> previous_remote_commit),
    UNIQUE (rollback_operation_id,previous_request_sha256)
);
REVOKE ALL ON public.knowledge_fleet_rollback_reconciliations FROM PUBLIC;
CREATE TRIGGER ygg_knowledge_fleet_reconciliation_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_fleet_rollback_reconciliations FOR EACH STATEMENT
    EXECUTE FUNCTION public.ygg_knowledge_migration_event_guard();
CREATE FUNCTION public.ygg_knowledge_fleet_reconciliation_check()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER SET search_path=pg_catalog,pg_temp AS $$
DECLARE
    reverse_op public.knowledge_fleet_rollbacks%ROWTYPE;
    forward_op public.knowledge_fleet_operations%ROWTYPE;
    fence public.knowledge_fleet_rollback_fences%ROWTYPE;
    marker public.knowledge_storage%ROWTYPE;
    predecessor TEXT;
    evidence JSONB;
    hosts UUID[];
BEGIN
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'reconciliation requires READ COMMITTED' USING ERRCODE='55000';
    END IF;
    PERFORM pg_advisory_xact_lock(1497843531,1);
    SELECT * INTO STRICT reverse_op FROM public.knowledge_fleet_rollbacks WHERE operation_id=NEW.rollback_operation_id;
    SELECT * INTO STRICT forward_op FROM public.knowledge_fleet_operations WHERE operation_id=reverse_op.forward_operation_id;
    SELECT * INTO STRICT fence FROM public.knowledge_fleet_rollback_fences WHERE operation_id=NEW.rollback_operation_id;
    SELECT * INTO STRICT marker FROM public.knowledge_storage WHERE singleton FOR UPDATE;
    IF marker.database_id IS DISTINCT FROM reverse_op.database_id OR marker.corpus_id IS DISTINCT FROM forward_op.corpus_id
        OR marker.backend IS DISTINCT FROM 'fenced' OR marker.generation IS DISTINCT FROM fence.generation
        OR fence.generation IS DISTINCT FROM reverse_op.source_generation+1
        OR NEW.hosts_sha256 IS DISTINCT FROM fence.hosts_sha256
        OR EXISTS (SELECT 1 FROM public.knowledge_reverse_receipts WHERE operation_id=NEW.rollback_operation_id)
        OR EXISTS (SELECT 1 FROM public.knowledge_fleet_rollback_cancellations WHERE operation_id=NEW.rollback_operation_id) THEN
        RAISE EXCEPTION 'reconciliation requires untouched owned reverse fence' USING ERRCODE='55000';
    END IF;
    IF NEW.previous_request_sha256=reverse_op.request_sha256 THEN
        predecessor := fence.remote_commit;
    ELSE
        SELECT remote_commit INTO STRICT predecessor FROM public.knowledge_fleet_rollback_reconciliations
            WHERE rollback_operation_id=NEW.rollback_operation_id AND request_sha256=NEW.previous_request_sha256;
    END IF;
    IF predecessor IS DISTINCT FROM NEW.previous_remote_commit
        OR EXISTS (SELECT 1 FROM public.knowledge_fleet_rollback_reconciliations WHERE rollback_operation_id=NEW.rollback_operation_id AND previous_request_sha256=NEW.previous_request_sha256) THEN
        RAISE EXCEPTION 'reconciliation predecessor is no longer current' USING ERRCODE='55000';
    END IF;
    evidence := NEW.request_json::jsonb;
    IF evidence->>'version' IS DISTINCT FROM '1'
        OR evidence->>'operation' IS DISTINCT FROM NEW.operation_id::text
        OR NEW.operation_id IN (reverse_op.operation_id,forward_op.operation_id,'00000000-0000-0000-0000-000000000000'::uuid)
        OR evidence->>'rollback_operation' IS DISTINCT FROM NEW.rollback_operation_id::text
        OR evidence->>'rollback_request_sha256' IS DISTINCT FROM reverse_op.request_sha256
        OR evidence->>'fenced_generation' IS DISTINCT FROM fence.generation::text
        OR evidence->>'previous_request_sha256' IS DISTINCT FROM NEW.previous_request_sha256
        OR evidence->>'previous_remote_commit' IS DISTINCT FROM NEW.previous_remote_commit
        OR evidence->>'expected_remote_commit' IS DISTINCT FROM NEW.remote_commit
        OR evidence->'all_participating_hosts_listed' IS DISTINCT FROM 'true'::jsonb
        OR evidence->'schema_changes_stopped' IS DISTINCT FROM 'true'::jsonb
        OR evidence->'session_preserving_endpoint' IS DISTINCT FROM 'true'::jsonb
        OR evidence->'remote_writers_stopped' IS DISTINCT FROM 'true'::jsonb
        OR jsonb_typeof(evidence->'participants') IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'reconciliation request evidence differs' USING ERRCODE='55000';
    END IF;
    SELECT array_agg((host->>'id')::uuid ORDER BY (host->>'id')::uuid) INTO hosts
        FROM jsonb_array_elements(evidence->'participants') host;
    IF hosts IS DISTINCT FROM forward_op.participants
        OR EXISTS (SELECT 1 FROM jsonb_array_elements(evidence->'participants') host
            WHERE host->'knowledge_writers_stopped' IS DISTINCT FROM 'true'::jsonb OR host->'external_editors_stopped' IS DISTINCT FROM 'true'::jsonb) THEN
        RAISE EXCEPTION 'reconciliation requires complete quiesced census' USING ERRCODE='55000';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER ygg_knowledge_fleet_reconciliation_check BEFORE INSERT
    ON public.knowledge_fleet_rollback_reconciliations FOR EACH ROW
    EXECUTE FUNCTION public.ygg_knowledge_fleet_reconciliation_check();

-- Older binaries cannot import the superseded snapshot after reconciliation.
CREATE FUNCTION public.ygg_knowledge_fleet_reconciled_import_check()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER SET search_path=pg_catalog,pg_temp AS $$
DECLARE selected_commit TEXT;
BEGIN
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'reconciled import requires READ COMMITTED' USING ERRCODE='55000';
    END IF;
    PERFORM pg_advisory_xact_lock(1497843531,1);
    SELECT remote_commit INTO selected_commit FROM public.knowledge_fleet_rollback_reconciliations r
        WHERE rollback_operation_id=NEW.operation_id AND NOT EXISTS
        (SELECT 1 FROM public.knowledge_fleet_rollback_reconciliations n WHERE n.rollback_operation_id=r.rollback_operation_id AND n.previous_request_sha256=r.request_sha256);
    IF selected_commit IS NOT NULL AND NEW.evidence->>'shared_commit' IS DISTINCT FROM selected_commit THEN
        RAISE EXCEPTION 'reverse import uses superseded reconciliation snapshot' USING ERRCODE='55000';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER ygg_knowledge_fleet_reconciled_import_check BEFORE INSERT
    ON public.knowledge_reverse_receipts FOR EACH ROW
    EXECUTE FUNCTION public.ygg_knowledge_fleet_reconciled_import_check();
