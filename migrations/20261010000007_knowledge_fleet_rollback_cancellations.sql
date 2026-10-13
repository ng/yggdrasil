-- Cancellation starts before any host is restored. Keep the generation's unique
-- rollback reservation until a later complete-host cancellation seal permits a
-- new operation; cancellation must never silently retarget the original request.
CREATE TABLE public.knowledge_fleet_rollback_cancellations (
    operation_id UUID PRIMARY KEY REFERENCES public.knowledge_fleet_rollbacks(operation_id),
    request_sha256 TEXT NOT NULL CHECK (request_sha256 ~ '^[0-9a-f]{64}$'),
    database_id UUID NOT NULL,
    source_generation BIGINT NOT NULL CHECK (source_generation > 0),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
REVOKE ALL ON public.knowledge_fleet_rollback_cancellations FROM PUBLIC;
CREATE TRIGGER ygg_knowledge_fleet_rollback_cancellation_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_fleet_rollback_cancellations FOR EACH STATEMENT
    EXECUTE FUNCTION public.ygg_knowledge_migration_event_guard();
CREATE FUNCTION public.ygg_knowledge_fleet_rollback_cancellation_check()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER SET search_path=pg_catalog,pg_temp AS $$
DECLARE
    reverse_op public.knowledge_fleet_rollbacks%ROWTYPE;
    forward_op public.knowledge_fleet_operations%ROWTYPE;
    marker public.knowledge_storage%ROWTYPE;
BEGIN
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'rollback cancellation guards require READ COMMITTED' USING ERRCODE='55000';
    END IF;
    PERFORM pg_advisory_xact_lock(1497843531,1);
    SELECT * INTO STRICT reverse_op FROM public.knowledge_fleet_rollbacks WHERE operation_id=NEW.operation_id;
    SELECT * INTO STRICT forward_op FROM public.knowledge_fleet_operations WHERE operation_id=reverse_op.forward_operation_id;
    SELECT * INTO STRICT marker FROM public.knowledge_storage WHERE singleton FOR UPDATE;
    IF NEW.request_sha256 IS DISTINCT FROM reverse_op.request_sha256
        OR NEW.database_id IS DISTINCT FROM reverse_op.database_id
        OR NEW.source_generation IS DISTINCT FROM reverse_op.source_generation
        OR marker.database_id IS DISTINCT FROM NEW.database_id
        OR marker.generation IS DISTINCT FROM NEW.source_generation
        OR marker.backend IS DISTINCT FROM 'okf'
        OR marker.corpus_id IS DISTINCT FROM forward_op.corpus_id
        OR EXISTS (SELECT 1 FROM public.knowledge_fleet_rollback_fences WHERE operation_id=NEW.operation_id) THEN
        RAISE EXCEPTION 'rollback cancellation requires original unfenced OKF generation and exact request' USING ERRCODE='55000';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER ygg_knowledge_fleet_rollback_cancellation_check BEFORE INSERT
    ON public.knowledge_fleet_rollback_cancellations FOR EACH ROW
    EXECUTE FUNCTION public.ygg_knowledge_fleet_rollback_cancellation_check();
CREATE FUNCTION public.ygg_knowledge_fleet_rollback_not_cancelled()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER SET search_path=pg_catalog,pg_temp AS $$
BEGIN
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'rollback cancellation guards require READ COMMITTED' USING ERRCODE='55000';
    END IF;
    PERFORM pg_advisory_xact_lock(1497843531,1);
    IF EXISTS (SELECT 1 FROM public.knowledge_fleet_rollback_cancellations WHERE operation_id=NEW.operation_id) THEN
        RAISE EXCEPTION 'rollback operation cancelled' USING ERRCODE='55000';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER ygg_knowledge_fleet_rollback_not_cancelled BEFORE INSERT
    ON public.knowledge_fleet_rollback_fences FOR EACH ROW
    EXECUTE FUNCTION public.ygg_knowledge_fleet_rollback_not_cancelled();
