-- Compatibility foundation. Phase changes are privileged migration actions;
-- publishing/validating the corresponding filesystem manifest is handled by the
-- cutover command, not inferred from this marker alone.
CREATE TABLE public.knowledge_storage (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    database_id UUID NOT NULL DEFAULT gen_random_uuid(),
    generation BIGINT NOT NULL DEFAULT 1 CHECK (generation >= 1),
    minimum_client INTEGER NOT NULL DEFAULT 1 CHECK (minimum_client >= 1),
    backend TEXT NOT NULL DEFAULT 'sql' CHECK (backend IN ('sql', 'fenced', 'okf')),
    corpus_id UUID,
    CHECK ((backend = 'sql' AND corpus_id IS NULL) OR (backend <> 'sql' AND corpus_id IS NOT NULL))
);
INSERT INTO public.knowledge_storage DEFAULT VALUES;
REVOKE ALL ON public.knowledge_storage FROM PUBLIC;
GRANT SELECT ON public.knowledge_storage TO PUBLIC;

-- Two-int advisory key reserved for knowledge storage transitions. Shared leases
-- live through the entire guarded operation; exclusive transitions drain them.
CREATE FUNCTION public.ygg_knowledge_guard(writing BOOLEAN, protocol INTEGER, expected_generation BIGINT DEFAULT NULL)
RETURNS BIGINT LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE marker public.knowledge_storage%ROWTYPE;
BEGIN
    PERFORM pg_advisory_xact_lock_shared(1497843531, 1);
    SELECT * INTO STRICT marker FROM public.knowledge_storage WHERE singleton FOR SHARE;
    IF writing IS NULL OR protocol IS NULL OR protocol < marker.minimum_client THEN
        RAISE EXCEPTION 'knowledge client protocol is too old; upgrade client' USING ERRCODE = '55000';
    END IF;
    IF expected_generation IS NOT NULL AND expected_generation <> marker.generation THEN
        RAISE EXCEPTION 'knowledge storage generation changed; reload configuration' USING ERRCODE = '55000';
    END IF;
    IF marker.backend = 'okf' OR (writing AND marker.backend = 'fenced') THEN
        RAISE EXCEPTION 'SQL knowledge access unavailable in storage phase %', marker.backend USING ERRCODE = '55000';
    END IF;
    RETURN marker.generation;
END $$;

CREATE FUNCTION public.ygg_knowledge_write_fence()
RETURNS TRIGGER LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE phase TEXT;
BEGIN
    PERFORM pg_advisory_xact_lock_shared(1497843531, 1);
    SELECT backend INTO STRICT phase FROM public.knowledge_storage WHERE singleton FOR SHARE;
    IF phase <> 'sql' THEN
        RAISE EXCEPTION 'legacy knowledge writes are fenced (%); use compatible client', phase USING ERRCODE = '55000';
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER ygg_knowledge_write_fence BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.memories FOR EACH STATEMENT EXECUTE FUNCTION public.ygg_knowledge_write_fence();
CREATE TRIGGER ygg_knowledge_write_fence BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.learnings FOR EACH STATEMENT EXECUTE FUNCTION public.ygg_knowledge_write_fence();

-- Acquire before the UPDATE executor locks a marker row: all paths take the
-- advisory lease before row locks. FOR SHARE above rejects an old repeatable-
-- read snapshot after a committed phase change instead of trusting stale state.
CREATE FUNCTION public.ygg_knowledge_marker_lease()
RETURNS TRIGGER LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(1497843531, 1);
    RETURN NULL;
END $$;
CREATE TRIGGER ygg_knowledge_marker_lease BEFORE UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_storage FOR EACH STATEMENT EXECUTE FUNCTION public.ygg_knowledge_marker_lease();

CREATE FUNCTION public.ygg_knowledge_marker_change()
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
    RETURN NEW;
END $$;
CREATE TRIGGER ygg_knowledge_marker_change BEFORE UPDATE OR DELETE
    ON public.knowledge_storage FOR EACH ROW EXECUTE FUNCTION public.ygg_knowledge_marker_change();

CREATE TRIGGER ygg_knowledge_marker_truncate BEFORE TRUNCATE
    ON public.knowledge_storage FOR EACH STATEMENT EXECUTE FUNCTION public.ygg_knowledge_marker_change();
