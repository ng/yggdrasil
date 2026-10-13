-- A receipt and its restored rows commit together. Operation IDs are chosen and
-- durably saved by the migration workflow before sending the apply transaction.
CREATE TABLE public.knowledge_reverse_receipts (
    operation_id UUID PRIMARY KEY,
    database_id UUID NOT NULL,
    corpus_id UUID NOT NULL,
    fenced_generation BIGINT NOT NULL CHECK (fenced_generation > 0),
    request_sha256 TEXT NOT NULL CHECK (request_sha256 ~ '^[0-9a-f]{64}$'),
    notes_sha256 TEXT NOT NULL CHECK (notes_sha256 ~ '^[0-9a-f]{64}$'),
    rules_sha256 TEXT NOT NULL CHECK (rules_sha256 ~ '^[0-9a-f]{64}$'),
    evidence JSONB NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
REVOKE ALL ON public.knowledge_reverse_receipts FROM PUBLIC;

CREATE FUNCTION public.ygg_knowledge_reverse_receipt_guard()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS $$
DECLARE owner_id OID;
BEGIN
    SELECT relowner INTO STRICT owner_id FROM pg_catalog.pg_class WHERE oid='public.knowledge_storage'::regclass;
    IF TG_OP <> 'INSERT' THEN
        RAISE EXCEPTION 'reverse-import receipts are immutable' USING ERRCODE='55000';
    END IF;
    IF NOT pg_has_role(session_user,owner_id,'USAGE') OR NOT pg_has_role(current_user,owner_id,'USAGE') THEN
        RAISE EXCEPTION 'reverse-import receipts require migration owner' USING ERRCODE='42501';
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER ygg_knowledge_reverse_receipt_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_reverse_receipts FOR EACH STATEMENT EXECUTE FUNCTION public.ygg_knowledge_reverse_receipt_guard();

CREATE FUNCTION public.ygg_knowledge_reverse_apply_once(
    operation UUID, expected_database UUID, expected_corpus UUID, expected_generation BIGINT,
    recovery_evidence JSONB, notes JSONB, rules JSONB
) RETURNS BOOLEAN LANGUAGE plpgsql SECURITY INVOKER SET search_path=pg_catalog,pg_temp SET timezone='UTC' AS $$
DECLARE owner_id OID; marker public.knowledge_storage%ROWTYPE;
    prior public.knowledge_reverse_receipts%ROWTYPE; request_hash TEXT; notes_hash TEXT; rules_hash TEXT;
    evidence_keys TEXT[];
BEGIN
    SELECT relowner INTO STRICT owner_id FROM pg_catalog.pg_class WHERE oid='public.knowledge_storage'::regclass;
    IF NOT pg_has_role(session_user,owner_id,'USAGE') OR NOT pg_has_role(current_user,owner_id,'USAGE') THEN
        RAISE EXCEPTION 'reverse import requires migration owner' USING ERRCODE='42501';
    END IF;
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'receipt recovery requires READ COMMITTED';
    END IF;
    PERFORM pg_advisory_xact_lock(1497843531,1);
    SELECT * INTO STRICT marker FROM public.knowledge_storage WHERE singleton FOR UPDATE;
    IF operation IS NULL OR marker.database_id IS DISTINCT FROM expected_database
       OR marker.corpus_id IS DISTINCT FROM expected_corpus OR marker.generation IS DISTINCT FROM expected_generation
       OR marker.backend <> 'fenced' OR marker.minimum_client > 1 THEN
        RAISE EXCEPTION 'receipt recovery requires expected compatible fenced generation' USING ERRCODE='55000';
    END IF;
    IF jsonb_typeof(recovery_evidence) IS DISTINCT FROM 'object'
       OR jsonb_typeof(notes) IS DISTINCT FROM 'array' OR jsonb_typeof(rules) IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'invalid reverse-import request';
    END IF;
    IF octet_length(recovery_evidence::text) > 4096 OR jsonb_array_length(notes)+jsonb_array_length(rules)>100000
       OR octet_length(notes::text)::bigint+octet_length(rules::text)::bigint > 67108864 THEN
        RAISE EXCEPTION 'reverse-import receipt request exceeds limits';
    END IF;
    SELECT array_agg(k ORDER BY k COLLATE "C") INTO evidence_keys FROM jsonb_object_keys(recovery_evidence) k;
    IF evidence_keys IS DISTINCT FROM ARRAY['candidate_sha256','corpus_revision','policy_revision','shared_commit','version']::TEXT[]
       OR recovery_evidence->'version' IS DISTINCT FROM '1'::jsonb
       OR jsonb_typeof(recovery_evidence->'candidate_sha256') IS DISTINCT FROM 'string'
       OR jsonb_typeof(recovery_evidence->'corpus_revision') IS DISTINCT FROM 'string'
       OR jsonb_typeof(recovery_evidence->'policy_revision') IS DISTINCT FROM 'string'
       OR jsonb_typeof(recovery_evidence->'shared_commit') NOT IN ('string','null')
       OR COALESCE(recovery_evidence->>'candidate_sha256','') !~ '^[0-9a-f]{64}$'
       OR COALESCE(recovery_evidence->>'corpus_revision','') !~ '^[0-9a-f]{64}$'
       OR COALESCE(recovery_evidence->>'policy_revision','') !~ '^[0-9a-f]{64}$'
       OR (recovery_evidence->'shared_commit' <> 'null'::jsonb AND COALESCE(recovery_evidence->>'shared_commit','') !~ '^([0-9a-f]{40}|[0-9a-f]{64})$') THEN
        RAISE EXCEPTION 'invalid reverse-import recovery evidence';
    END IF;
    request_hash := encode(sha256(convert_to(jsonb_build_object('database',expected_database,'corpus',expected_corpus,
        'generation',expected_generation,'evidence',recovery_evidence,'notes',notes,'rules',rules)::text,'UTF8')),'hex');
    SELECT * INTO prior FROM public.knowledge_reverse_receipts WHERE operation_id=operation;
    IF FOUND THEN
        IF prior.database_id IS DISTINCT FROM expected_database OR prior.corpus_id IS DISTINCT FROM expected_corpus
           OR prior.fenced_generation IS DISTINCT FROM expected_generation OR prior.request_sha256 <> request_hash THEN
            RAISE EXCEPTION 'operation ID belongs to a different reverse-import request';
        END IF;
    ELSE
        PERFORM public.ygg_knowledge_reverse_import(expected_database,expected_corpus,expected_generation,notes,rules);
    END IF;
    SELECT encode(sha256(convert_to(COALESCE(jsonb_agg(to_jsonb(n) ORDER BY n.memory_id),'[]'::jsonb)::text,'UTF8')),'hex')
      INTO notes_hash FROM public.memories n;
    SELECT encode(sha256(convert_to(COALESCE(jsonb_agg(to_jsonb(r) ORDER BY r.learning_id),'[]'::jsonb)::text,'UTF8')),'hex')
      INTO rules_hash FROM public.learnings r;
    IF prior.operation_id IS NOT NULL THEN
        IF notes_hash <> prior.notes_sha256 OR rules_hash <> prior.rules_sha256 THEN
            RAISE EXCEPTION 'restored rows changed after recorded reverse import';
        END IF;
        RETURN FALSE;
    END IF;
    INSERT INTO public.knowledge_reverse_receipts(operation_id,database_id,corpus_id,fenced_generation,request_sha256,notes_sha256,rules_sha256,evidence)
      VALUES(operation,expected_database,expected_corpus,expected_generation,request_hash,notes_hash,rules_hash,recovery_evidence);
    RETURN TRUE;
END $$;
REVOKE ALL ON FUNCTION public.ygg_knowledge_reverse_apply_once(UUID,UUID,UUID,BIGINT,JSONB,JSONB,JSONB) FROM PUBLIC;
