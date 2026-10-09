-- Compatibility declarations are operational metadata. Connection startup must
-- remain available to coordination while knowledge migration holds its lease.
-- A live-client audit is an observation, not a connection-admission fence.
CREATE OR REPLACE FUNCTION public.ygg_knowledge_register_client(
    client_protocol INTEGER, client_version TEXT, client_process UUID, started TIMESTAMPTZ
) RETURNS VOID LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp AS $$
DECLARE actor OID;
BEGIN
    IF client_protocol IS NULL OR client_protocol <= 0 OR client_version IS NULL
       OR octet_length(client_version) NOT BETWEEN 1 AND 128
       OR client_process IS NULL OR client_process = '00000000-0000-0000-0000-000000000000'::uuid
       OR started IS NULL OR started > clock_timestamp() THEN
        RAISE EXCEPTION 'invalid knowledge client registration' USING ERRCODE = '22023';
    END IF;
    SELECT oid INTO STRICT actor FROM pg_catalog.pg_roles WHERE rolname = session_user;
    -- PID/datid remain observable without query-text/statistics privileges.
    -- Remove ended connections; retain all still-live PIDs conservatively.
    DELETE FROM public.knowledge_clients c
      WHERE NOT EXISTS (SELECT 1 FROM pg_catalog.pg_stat_activity a
                        WHERE a.pid = c.backend_pid AND a.datid =
                          (SELECT oid FROM pg_catalog.pg_database WHERE datname = current_database()))
         OR (c.backend_pid = pg_backend_pid() AND c.backend_start <> started);
    INSERT INTO public.knowledge_clients(backend_pid,backend_start,role_oid,process_id,protocol,binary_version)
      VALUES(pg_backend_pid(),started,actor,client_process,client_protocol,client_version)
      ON CONFLICT (backend_pid,backend_start) DO NOTHING;
    IF NOT EXISTS (SELECT 1 FROM public.knowledge_clients
                   WHERE backend_pid=pg_backend_pid() AND backend_start=started
                     AND role_oid=actor AND process_id=client_process
                     AND protocol=client_protocol AND binary_version=client_version) THEN
        RAISE EXCEPTION 'backend already registered to a different client; session affinity required'
            USING ERRCODE = '55000';
    END IF;
END $$;
