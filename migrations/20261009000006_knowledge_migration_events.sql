-- Ownership of a maintenance fence and its optional pre-activation abort are
-- committed with the marker. A matching marker alone cannot identify an operation.
CREATE TABLE public.knowledge_migration_events (
    operation_id UUID NOT NULL,
    step TEXT NOT NULL CHECK (step IN ('fenced','aborted')),
    request_sha256 TEXT NOT NULL CHECK (request_sha256 ~ '^[0-9a-f]{64}$'),
    database_id UUID NOT NULL,
    target_generation BIGINT NOT NULL CHECK (target_generation > 0),
    corpus_id UUID,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (operation_id,step),
    UNIQUE (database_id,target_generation),
    CHECK ((step='fenced') = (corpus_id IS NOT NULL))
);
REVOKE ALL ON public.knowledge_migration_events FROM PUBLIC;
CREATE FUNCTION public.ygg_knowledge_migration_event_guard()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER SET search_path=pg_catalog,pg_temp AS $$
DECLARE owner_id OID;
BEGIN
    SELECT relowner INTO STRICT owner_id FROM pg_catalog.pg_class WHERE oid='public.knowledge_storage'::regclass;
    IF TG_OP <> 'INSERT' THEN
        RAISE EXCEPTION 'knowledge migration events are immutable' USING ERRCODE='55000';
    END IF;
    IF NOT pg_has_role(session_user,owner_id,'USAGE') OR NOT pg_has_role(current_user,owner_id,'USAGE') THEN
        RAISE EXCEPTION 'knowledge migration events require migration owner' USING ERRCODE='42501';
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER ygg_knowledge_migration_event_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_migration_events FOR EACH STATEMENT EXECUTE FUNCTION public.ygg_knowledge_migration_event_guard();
