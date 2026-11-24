-- rusty:breaking a check constraint is replaced by a wider one (old binaries would keep working, but the lint cannot tell a relaxation from a restriction)
-- The calendar and contacts service (docs/design-caldav-carddav.md) writes a heartbeat too: its name is allowed and the definer function maps its role to it, as for the other services.
ALTER TABLE service_heartbeats DROP CONSTRAINT service_heartbeats_service_valid;
ALTER TABLE service_heartbeats ADD CONSTRAINT service_heartbeats_service_valid CHECK (service IN ('smtp-in', 'submission', 'imap', 'outbound', 'gc', 'admin', 'webmail', 'dav', 'dns'));

CREATE OR REPLACE FUNCTION rs_heartbeat(p_service text, p_instance text, p_version text, p_uptime_secs bigint, p_status text, p_detail text) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE
  svc text;
BEGIN
  svc := CASE session_user
    WHEN 'rusty_smtp_in' THEN 'smtp-in'
    WHEN 'rusty_submission' THEN 'submission'
    WHEN 'rusty_imap' THEN 'imap'
    WHEN 'rusty_outbound' THEN 'outbound'
    WHEN 'rusty_gc' THEN 'gc'
    WHEN 'rusty_admin' THEN 'admin'
    WHEN 'rusty_webmail' THEN 'webmail'
    WHEN 'rusty_dav' THEN 'dav'
    WHEN 'rusty_dns' THEN 'dns'
    ELSE p_service
  END;
  INSERT INTO service_heartbeats (service, instance, version, started_at, last_seen, status, detail)
  VALUES (svc, p_instance, p_version, rs_now() - greatest(p_uptime_secs, 0), rs_now(), p_status, p_detail)
  ON CONFLICT (service, instance) DO UPDATE
    SET version = EXCLUDED.version, started_at = EXCLUDED.started_at, last_seen = EXCLUDED.last_seen, status = EXCLUDED.status, detail = EXCLUDED.detail;
END;
$$;
INSERT INTO schema_compat (version, min_binary) VALUES (59, 59);
