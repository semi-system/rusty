-- The heartbeat of every running service (docs/design-admin-monitor.md): one row per (service, instance), renewed every few seconds, read by the admin web's monitor page.
-- `last_seen` comes from the database clock (rs_now()), never from the service's own. `instance` is the container's host name, so every replica of a service has its own row.
CREATE TABLE service_heartbeats (
  service    text COLLATE "C" NOT NULL
             CONSTRAINT service_heartbeats_service_valid CHECK (service IN ('smtp-in', 'submission', 'imap', 'outbound', 'gc', 'admin')),
  instance   text COLLATE "C" NOT NULL CONSTRAINT service_heartbeats_instance_valid CHECK (length(instance) BETWEEN 1 AND 128),
  version    text COLLATE "C" NOT NULL CONSTRAINT service_heartbeats_version_valid CHECK (length(version) BETWEEN 1 AND 64),
  started_at bigint NOT NULL,
  last_seen  bigint NOT NULL,
  status     text COLLATE "C" NOT NULL CONSTRAINT service_heartbeats_status_valid CHECK (status IN ('ok', 'degraded')),
  detail     text COLLATE "C" NOT NULL DEFAULT '{}' CONSTRAINT service_heartbeats_detail_valid CHECK (length(detail) <= 2048),
  PRIMARY KEY (service, instance)
);

-- The cleanup removes rows not seen for a day.
CREATE INDEX service_heartbeats_seen ON service_heartbeats (last_seen);

-- The only way to write a heartbeat. SECURITY DEFINER: the service roles hold EXECUTE on it and no write on the table. For a service role the service name is the role's own
-- (`rusty_imap` writes `imap`, whatever it passes), so a compromised service cannot write another service's row. Any other login that may call it (the operator's role, the
-- development setup, the test harness) names the service itself. The login is `session_user`: inside a definer function `current_user` is the owner.
CREATE FUNCTION rs_heartbeat(p_service text, p_instance text, p_version text, p_uptime_secs bigint, p_status text, p_detail text) RETURNS void
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
    ELSE p_service
  END;
  INSERT INTO service_heartbeats (service, instance, version, started_at, last_seen, status, detail)
  VALUES (svc, p_instance, p_version, rs_now() - greatest(p_uptime_secs, 0), rs_now(), p_status, p_detail)
  ON CONFLICT (service, instance) DO UPDATE
    SET version = EXCLUDED.version, started_at = EXCLUDED.started_at, last_seen = EXCLUDED.last_seen, status = EXCLUDED.status, detail = EXCLUDED.detail;
END;
$$;
REVOKE ALL ON FUNCTION rs_heartbeat(text, text, text, bigint, text, text) FROM PUBLIC;
