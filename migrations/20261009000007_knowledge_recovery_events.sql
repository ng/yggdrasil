-- Bind the reverse fence and final SQL activation to durable recovery requests.
CREATE TABLE public.knowledge_recovery_events (
    operation_id UUID NOT NULL,
    step TEXT NOT NULL CHECK (step IN ('fenced','sql')),
    request_sha256 TEXT NOT NULL CHECK (request_sha256 ~ '^[0-9a-f]{64}$'),
    database_id UUID NOT NULL,
    corpus_id UUID NOT NULL,
    generation BIGINT NOT NULL CHECK (generation > 0),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (operation_id,step),
    UNIQUE (database_id,generation)
);
REVOKE ALL ON public.knowledge_recovery_events FROM PUBLIC;
CREATE FUNCTION public.ygg_knowledge_recovery_event_guard()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER SET search_path=pg_catalog,pg_temp AS $$
DECLARE owner_id OID;
BEGIN
    SELECT relowner INTO STRICT owner_id FROM pg_catalog.pg_class WHERE oid='public.knowledge_storage'::regclass;
    IF TG_OP <> 'INSERT' THEN
        RAISE EXCEPTION 'knowledge recovery events are immutable' USING ERRCODE='55000';
    END IF;
    IF NOT pg_has_role(session_user,owner_id,'USAGE') OR NOT pg_has_role(current_user,owner_id,'USAGE') THEN
        RAISE EXCEPTION 'knowledge recovery events require migration owner' USING ERRCODE='42501';
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER ygg_knowledge_recovery_event_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_recovery_events FOR EACH STATEMENT EXECUTE FUNCTION public.ygg_knowledge_recovery_event_guard();
