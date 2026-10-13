-- Records only the database half of a forward cutover. Publication, local
-- selections and participating-host quiescence are migration-workflow evidence.
CREATE TABLE public.knowledge_forward_receipts (
    operation_id UUID PRIMARY KEY,
    database_id UUID NOT NULL,
    corpus_id UUID NOT NULL,
    fenced_generation BIGINT NOT NULL CHECK (fenced_generation > 0),
    active_generation BIGINT NOT NULL CHECK (active_generation = fenced_generation + 1),
    manifest_sha256 TEXT NOT NULL CHECK (manifest_sha256 ~ '^[0-9a-f]{64}$'),
    activated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (database_id, active_generation)
);
REVOKE ALL ON public.knowledge_forward_receipts FROM PUBLIC;

CREATE FUNCTION public.ygg_knowledge_forward_receipt_guard()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS $$
DECLARE owner_id OID;
BEGIN
    SELECT relowner INTO STRICT owner_id FROM pg_catalog.pg_class WHERE oid='public.knowledge_storage'::regclass;
    IF TG_OP <> 'INSERT' THEN
        RAISE EXCEPTION 'forward-cutover receipts are immutable' USING ERRCODE='55000';
    END IF;
    IF NOT pg_has_role(session_user,owner_id,'USAGE') OR NOT pg_has_role(current_user,owner_id,'USAGE') THEN
        RAISE EXCEPTION 'forward-cutover receipts require migration owner' USING ERRCODE='42501';
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER ygg_knowledge_forward_receipt_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_forward_receipts FOR EACH STATEMENT EXECUTE FUNCTION public.ygg_knowledge_forward_receipt_guard();
