-- UTF8 server semantics are required for Unicode OKF matching. Ordinary legacy
-- SQL reads/writes and fenced-to-SQL abort remain available in other encodings.
-- Extend existing guards without changing their leases, role checks or grants.
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
    IF phase = 'fenced' AND migration AND current_setting('server_encoding') <> 'UTF8' THEN
        RAISE EXCEPTION 'OKF migration requires UTF8 server encoding to preserve Unicode matching; selected database uses %', current_setting('server_encoding') USING ERRCODE = '0A000';
    END IF;
    RETURN NULL;
END $$;

CREATE OR REPLACE FUNCTION public.ygg_knowledge_marker_change()
RETURNS TRIGGER LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(1497843531, 1);
    IF TG_OP IN ('DELETE', 'TRUNCATE') THEN
        RAISE EXCEPTION 'knowledge storage marker cannot be deleted' USING ERRCODE = '55000';
    END IF;
    IF NEW.database_id <> OLD.database_id OR NEW.generation <> OLD.generation + 1
        OR NEW.minimum_client < OLD.minimum_client THEN
        RAISE EXCEPTION 'knowledge transition requires stable database identity and next generation' USING ERRCODE = '55000';
    END IF;
    IF NOT ((OLD.backend = 'sql' AND NEW.backend IN ('sql', 'fenced'))
        OR (OLD.backend = 'fenced' AND NEW.backend IN ('sql', 'fenced', 'okf'))
        OR (OLD.backend = 'okf' AND NEW.backend IN ('okf', 'fenced'))) THEN
        RAISE EXCEPTION 'knowledge transition must pass through fenced phase' USING ERRCODE = '55000';
    END IF;
    IF OLD.backend = 'okf' AND NEW.corpus_id IS DISTINCT FROM OLD.corpus_id THEN
        RAISE EXCEPTION 'active corpus identity cannot change during transition' USING ERRCODE = '55000';
    END IF;
    IF NEW.backend = 'okf' AND current_setting('server_encoding') <> 'UTF8' THEN
        RAISE EXCEPTION 'OKF migration requires UTF8 server encoding to preserve Unicode matching; selected database uses %', current_setting('server_encoding') USING ERRCODE = '0A000';
    END IF;
    RETURN NEW;
END $$;
