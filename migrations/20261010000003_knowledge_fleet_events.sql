-- Fleet-only transition evidence cannot adopt a private migration's fence.
CREATE TABLE public.knowledge_fleet_events (
    operation_id UUID NOT NULL REFERENCES public.knowledge_fleet_operations(operation_id),
    step TEXT NOT NULL CHECK (step IN ('fenced', 'aborted')),
    prepared_sha256 TEXT NOT NULL CHECK (prepared_sha256 ~ '^[0-9a-f]{64}$'),
    backup_sha256 TEXT NOT NULL CHECK (backup_sha256 ~ '^[0-9a-f]{64}$'),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (operation_id, step)
);
REVOKE ALL ON public.knowledge_fleet_events FROM PUBLIC;
CREATE TRIGGER ygg_knowledge_fleet_event_guard BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
    ON public.knowledge_fleet_events FOR EACH STATEMENT
    EXECUTE FUNCTION public.ygg_knowledge_migration_event_guard();
