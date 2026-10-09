-- Explicit owner migration path; runtime flags alone never bypass the fence.
CREATE OR REPLACE FUNCTION public.ygg_knowledge_write_fence()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE phase TEXT; owner_id OID; migration BOOLEAN;
BEGIN
    SELECT relowner INTO STRICT owner_id FROM pg_catalog.pg_class WHERE oid = 'public.knowledge_storage'::regclass;
    migration := COALESCE(current_setting('ygg.knowledge_reverse_import', true), 'off') = 'on'
        AND pg_has_role(session_user, owner_id, 'USAGE');
    IF migration THEN
        PERFORM pg_advisory_xact_lock(1497843531, 1);
    ELSE
        PERFORM pg_advisory_xact_lock_shared(1497843531, 1);
    END IF;
    SELECT backend INTO STRICT phase FROM public.knowledge_storage WHERE singleton FOR SHARE;
    IF phase <> 'sql' AND NOT (phase = 'fenced' AND migration) THEN
        RAISE EXCEPTION 'legacy knowledge writes are fenced (%); use compatible client', phase USING ERRCODE = '55000';
    END IF;
    RETURN NULL;
END $$;

-- Caller must retain current-bundle/usage evidence and quiesce file writers.
-- This function changes only SQL rows, never the storage marker/configuration.
CREATE FUNCTION public.ygg_knowledge_reverse_import(
    expected_database UUID, expected_corpus UUID, expected_generation BIGINT,
    notes JSONB, rules JSONB
) RETURNS VOID LANGUAGE plpgsql SECURITY INVOKER
SET search_path = pg_catalog, pg_temp SET timezone = 'UTC' AS $$
DECLARE marker public.knowledge_storage%ROWTYPE; owner_id OID;
    previous_flag TEXT; item JSONB; normalized JSONB; timestamp_key TEXT; expected_keys TEXT[]; actual_keys TEXT[];
    expected_notes JSONB; expected_rules JSONB; actual_rows JSONB;
BEGIN
    SELECT relowner INTO STRICT owner_id FROM pg_catalog.pg_class WHERE oid = 'public.knowledge_storage'::regclass;
    IF NOT pg_has_role(session_user, owner_id, 'USAGE')
       OR NOT pg_has_role(current_user, owner_id, 'USAGE') THEN
        RAISE EXCEPTION 'reverse import requires the migration owner' USING ERRCODE = '42501';
    END IF;
    PERFORM pg_advisory_xact_lock(1497843531, 1);
    SELECT * INTO STRICT marker FROM public.knowledge_storage WHERE singleton FOR UPDATE;
    IF marker.database_id IS DISTINCT FROM expected_database
       OR marker.corpus_id IS DISTINCT FROM expected_corpus
       OR marker.generation IS DISTINCT FROM expected_generation
       OR marker.backend <> 'fenced' OR marker.minimum_client > 1 THEN
        RAISE EXCEPTION 'reverse import requires the expected compatible fenced generation' USING ERRCODE = '55000';
    END IF;
    -- Stabilize row types without conflicting with an unaware writer's table
    -- RowExclusive lock while that writer waits on our advisory fence.
    LOCK TABLE public.memories, public.learnings IN ACCESS SHARE MODE;
    IF jsonb_typeof(notes) IS DISTINCT FROM 'array' OR jsonb_typeof(rules) IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'reverse import requires complete row arrays';
    END IF;
    IF jsonb_array_length(notes) + jsonb_array_length(rules) > 100000
       OR octet_length(notes::text)::bigint + octet_length(rules::text)::bigint > 67108864 THEN
        RAISE EXCEPTION 'reverse import exceeds size limit';
    END IF;
    expected_keys := ARRAY['created_at','created_by','memory_id','repo_id','text','user_id']::TEXT[];
    SELECT array_agg(attname::text ORDER BY attname::text COLLATE "C") INTO actual_keys
      FROM pg_catalog.pg_attribute WHERE attrelid = 'public.memories'::regclass AND attnum > 0 AND NOT attisdropped;
    IF actual_keys IS DISTINCT FROM expected_keys THEN
        RAISE EXCEPTION 'unsupported memories schema for reverse import';
    END IF;
    FOR item IN SELECT value FROM jsonb_array_elements(notes) LOOP
        IF jsonb_typeof(item) IS DISTINCT FROM 'object' THEN RAISE EXCEPTION 'invalid memories row'; END IF;
        SELECT array_agg(k ORDER BY k COLLATE "C") INTO actual_keys FROM jsonb_object_keys(item) AS k;
        IF actual_keys IS DISTINCT FROM expected_keys THEN RAISE EXCEPTION 'missing or unknown memories fields'; END IF;
        SELECT to_jsonb(r) INTO normalized FROM jsonb_populate_record(NULL::public.memories, item) r;
        IF (item - ARRAY['created_at']::TEXT[]) IS DISTINCT FROM (normalized - ARRAY['created_at']::TEXT[]) THEN
            RAISE EXCEPTION 'unrepresentable memories field types';
        END IF;
        FOREACH timestamp_key IN ARRAY ARRAY['created_at']::TEXT[] LOOP
            IF item->>timestamp_key IS NOT NULL AND (
                NOT isfinite((item->>timestamp_key)::timestamptz)
                OR item->>timestamp_key ~ ':[0-9]{2}:60([. Zz+-]|$)'
                OR substring(substring(item->>timestamp_key FROM '\.([0-9]+)') FROM 7) ~ '[1-9]') THEN
                RAISE EXCEPTION 'timestamp cannot be represented without precision loss';
            END IF;
        END LOOP;
    END LOOP;
    expected_keys := ARRAY['applied_count','approved_at','approved_by','context','created_at','created_by','file_glob','last_applied_at','learning_id','repo_id','rule_id','scope_tags','source','status','text','user_id']::TEXT[];
    SELECT array_agg(attname::text ORDER BY attname::text COLLATE "C") INTO actual_keys
      FROM pg_catalog.pg_attribute WHERE attrelid = 'public.learnings'::regclass AND attnum > 0 AND NOT attisdropped;
    IF actual_keys IS DISTINCT FROM expected_keys THEN
        RAISE EXCEPTION 'unsupported learnings schema for reverse import';
    END IF;
    FOR item IN SELECT value FROM jsonb_array_elements(rules) LOOP
        IF jsonb_typeof(item) IS DISTINCT FROM 'object' THEN RAISE EXCEPTION 'invalid learnings row'; END IF;
        SELECT array_agg(k ORDER BY k COLLATE "C") INTO actual_keys FROM jsonb_object_keys(item) AS k;
        IF actual_keys IS DISTINCT FROM expected_keys THEN RAISE EXCEPTION 'missing or unknown learnings fields'; END IF;
        SELECT to_jsonb(r) INTO normalized FROM jsonb_populate_record(NULL::public.learnings, item) r;
        IF (item - ARRAY['created_at','last_applied_at','approved_at']::TEXT[]) IS DISTINCT FROM (normalized - ARRAY['created_at','last_applied_at','approved_at']::TEXT[]) THEN
            RAISE EXCEPTION 'unrepresentable learnings field types';
        END IF;
        FOREACH timestamp_key IN ARRAY ARRAY['created_at','last_applied_at','approved_at']::TEXT[] LOOP
            IF item->>timestamp_key IS NOT NULL AND (
                NOT isfinite((item->>timestamp_key)::timestamptz)
                OR item->>timestamp_key ~ ':[0-9]{2}:60([. Zz+-]|$)'
                OR substring(substring(item->>timestamp_key FROM '\.([0-9]+)') FROM 7) ~ '[1-9]') THEN
                RAISE EXCEPTION 'timestamp cannot be represented without precision loss';
            END IF;
        END LOOP;
    END LOOP;
    IF EXISTS (SELECT 1 FROM (
        SELECT (n->>'memory_id')::uuid AS id FROM jsonb_array_elements(notes) n
        UNION ALL SELECT (r->>'learning_id')::uuid FROM jsonb_array_elements(rules) r
    ) ids GROUP BY id HAVING count(*) > 1 OR id IS NULL) THEN
        RAISE EXCEPTION 'duplicate or null reverse-import UUID';
    END IF;
    SELECT COALESCE(jsonb_agg(to_jsonb(n) ORDER BY n.memory_id), '[]'::jsonb) INTO expected_notes
      FROM jsonb_populate_recordset(NULL::public.memories, notes) n;
    SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY r.learning_id), '[]'::jsonb) INTO expected_rules
      FROM jsonb_populate_recordset(NULL::public.learnings, rules) r;
    previous_flag := COALESCE(current_setting('ygg.knowledge_reverse_import', true), 'off');
    PERFORM set_config('ygg.knowledge_reverse_import', 'on', true);
    DELETE FROM public.memories existing WHERE NOT EXISTS (
        SELECT 1 FROM jsonb_populate_recordset(NULL::public.memories, notes) desired WHERE desired.memory_id=existing.memory_id);
    INSERT INTO public.memories SELECT * FROM jsonb_populate_recordset(NULL::public.memories, notes)
        ON CONFLICT (memory_id) DO UPDATE SET repo_id=EXCLUDED.repo_id, text=EXCLUDED.text, created_by=EXCLUDED.created_by, created_at=EXCLUDED.created_at, user_id=EXCLUDED.user_id;
    SELECT COALESCE(jsonb_agg(to_jsonb(actual) ORDER BY actual.memory_id), '[]'::jsonb) INTO actual_rows FROM public.memories actual;
    IF actual_rows IS DISTINCT FROM expected_notes THEN RAISE EXCEPTION 'memories reverse-import parity failed'; END IF;
    DELETE FROM public.learnings existing WHERE NOT EXISTS (
        SELECT 1 FROM jsonb_populate_recordset(NULL::public.learnings, rules) desired WHERE desired.learning_id=existing.learning_id);
    -- JSON null is a valid JSONB tag value, distinct from SQL NULL. Preserve
    -- it directly from the input instead of jsonb_populate_record's NULL cast.
    INSERT INTO public.learnings (learning_id,repo_id,file_glob,rule_id,text,context,created_by,created_at,applied_count,scope_tags,user_id,status,source,approved_at,approved_by,last_applied_at)
        SELECT r.learning_id,r.repo_id,r.file_glob,r.rule_id,r.text,r.context,r.created_by,r.created_at,r.applied_count,
               source.value->'scope_tags',r.user_id,r.status,r.source,r.approved_at,r.approved_by,r.last_applied_at
        FROM jsonb_array_elements(rules) source
        CROSS JOIN LATERAL jsonb_populate_record(NULL::public.learnings, source.value) r
        ON CONFLICT (learning_id) DO UPDATE SET repo_id=EXCLUDED.repo_id, file_glob=EXCLUDED.file_glob, rule_id=EXCLUDED.rule_id, text=EXCLUDED.text, context=EXCLUDED.context, created_by=EXCLUDED.created_by, created_at=EXCLUDED.created_at, applied_count=EXCLUDED.applied_count, scope_tags=EXCLUDED.scope_tags, user_id=EXCLUDED.user_id, status=EXCLUDED.status, source=EXCLUDED.source, approved_at=EXCLUDED.approved_at, approved_by=EXCLUDED.approved_by, last_applied_at=EXCLUDED.last_applied_at;
    SELECT COALESCE(jsonb_agg(to_jsonb(actual) ORDER BY actual.learning_id), '[]'::jsonb) INTO actual_rows FROM public.learnings actual;
    IF actual_rows IS DISTINCT FROM expected_rules THEN RAISE EXCEPTION 'learnings reverse-import parity failed'; END IF;
    PERFORM set_config('ygg.knowledge_reverse_import', previous_flag, true);
END $$;
REVOKE ALL ON FUNCTION public.ygg_knowledge_reverse_import(UUID, UUID, BIGINT, JSONB, JSONB) FROM PUBLIC;
