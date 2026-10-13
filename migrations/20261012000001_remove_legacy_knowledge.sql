-- M7 (ADR 0019): OKF is the only knowledge backend. Remove the legacy SQL
-- knowledge tables, their write fence and the reverse-import functions.
-- The knowledge_storage marker and its lease/transition functions stay: they
-- still bind the selected OKF generation to this database.
DO $$
DECLARE
    marker_backend TEXT;
    note_rows BIGINT := 0;
    rule_rows BIGINT := 0;
    legacy_rows BIGINT := 0;
BEGIN
    SELECT backend INTO marker_backend FROM public.knowledge_storage WHERE singleton;
    IF to_regclass('public.memories') IS NOT NULL THEN
        EXECUTE 'SELECT count(*) FROM public.memories' INTO note_rows;
    END IF;
    IF to_regclass('public.learnings') IS NOT NULL THEN
        EXECUTE 'SELECT count(*) FROM public.learnings' INTO rule_rows;
    END IF;
    legacy_rows := note_rows + rule_rows;
    -- Refuse to discard knowledge that was never exported: an installation
    -- still reading SQL must cut over with a pre-M7 release first.
    IF legacy_rows > 0 AND coalesce(marker_backend, 'sql') <> 'okf' THEN
        RAISE EXCEPTION 'legacy knowledge rows exist but OKF is not the selected backend; cut over with a release that still ships `ygg knowledge migrate` before applying this migration'
            USING ERRCODE = '55000';
    END IF;
END $$;

DROP TRIGGER IF EXISTS ygg_knowledge_write_fence ON public.memories;
DROP TRIGGER IF EXISTS ygg_knowledge_write_fence ON public.learnings;
DROP TABLE IF EXISTS public.learnings;
DROP TABLE IF EXISTS public.memories;
DROP FUNCTION IF EXISTS public.ygg_knowledge_write_fence();
DROP FUNCTION IF EXISTS public.ygg_knowledge_guard(boolean, integer, bigint);
DROP FUNCTION IF EXISTS public.ygg_knowledge_reverse_import(uuid, uuid, bigint, jsonb, jsonb);
DROP FUNCTION IF EXISTS public.ygg_knowledge_reverse_apply_once(uuid, uuid, uuid, bigint, jsonb, jsonb, jsonb);
