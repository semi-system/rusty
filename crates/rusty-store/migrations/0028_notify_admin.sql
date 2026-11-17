-- Live updates of the admin console (docs/design-live-updates.md, section 5): what another process wrote (the outbound service recording a storage, DNS or relay check; another replica of
-- the admin service; `rustyctl`) is announced to every admin process, which tells the open pages. Like 0017 the database announces it at COMMIT, so a rolled-back change is never announced and
-- no application code has to remember to call it.
--
--   channel rusty_admin   payload '<kind>:<domain id>'      kind: domains accounts dns outbound storage admins; the id may be empty (the kind has none)
--
-- A notification is a hint, never data: it names what changed and nothing else. The triggers are row triggers; Postgres delivers one notification for identical payloads of one transaction,
-- so a statement that changes two hundred rows of a domain announces that domain once. The functions run as the caller (no SECURITY DEFINER: pg_notify needs no privilege) and pin
-- search_path with pg_temp last, as the functions of 0016 and 0017 do. The one function that reads another table (`rs_notify_admin_backend` reads the store of a backend) is only called by
-- the roles that already read that table.

CREATE FUNCTION rs_notify_admin() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
  d text;
BEGIN
  -- TG_ARGV[0] is the kind, TG_ARGV[1] the column that holds the domain's id (empty: the kind has none).
  IF TG_ARGV[1] <> '' THEN
    IF TG_OP = 'DELETE' THEN d := to_jsonb(OLD) ->> TG_ARGV[1]; ELSE d := to_jsonb(NEW) ->> TG_ARGV[1]; END IF;
  END IF;
  PERFORM pg_notify('rusty_admin', TG_ARGV[0] || ':' || COALESCE(d, ''));
  RETURN NULL;
END
$$;

-- A backend belongs to a store, and the store to a domain.
CREATE FUNCTION rs_notify_admin_backend() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
  d bigint;
BEGIN
  SELECT domain_id INTO d FROM blob_stores WHERE id = CASE WHEN TG_OP = 'DELETE' THEN OLD.store_id ELSE NEW.store_id END;
  PERFORM pg_notify('rusty_admin', 'storage:' || COALESCE(d::text, ''));
  RETURN NULL;
END
$$;

CREATE TRIGGER domains_notify_admin AFTER INSERT OR UPDATE OR DELETE ON domains
  FOR EACH ROW EXECUTE FUNCTION rs_notify_admin('domains', '');
CREATE TRIGGER accounts_notify_admin AFTER INSERT OR UPDATE OR DELETE ON accounts
  FOR EACH ROW EXECUTE FUNCTION rs_notify_admin('accounts', 'domain_id');
CREATE TRIGGER aliases_notify_admin AFTER INSERT OR UPDATE OR DELETE ON aliases
  FOR EACH ROW EXECUTE FUNCTION rs_notify_admin('accounts', 'domain_id');
CREATE TRIGGER domain_dns_state_notify_admin AFTER INSERT OR UPDATE OR DELETE ON domain_dns_state
  FOR EACH ROW EXECUTE FUNCTION rs_notify_admin('dns', 'domain_id');
CREATE TRIGGER domain_dkim_keys_notify_admin AFTER INSERT OR UPDATE OR DELETE ON domain_dkim_keys
  FOR EACH ROW EXECUTE FUNCTION rs_notify_admin('dns', 'domain_id');
CREATE TRIGGER domain_relays_notify_admin AFTER INSERT OR UPDATE OR DELETE ON domain_relays
  FOR EACH ROW EXECUTE FUNCTION rs_notify_admin('outbound', 'domain_id');
CREATE TRIGGER blob_stores_notify_admin AFTER INSERT OR UPDATE OR DELETE ON blob_stores
  FOR EACH ROW EXECUTE FUNCTION rs_notify_admin('storage', 'domain_id');
CREATE TRIGGER blob_backends_notify_admin AFTER INSERT OR UPDATE OR DELETE ON blob_backends
  FOR EACH ROW EXECUTE FUNCTION rs_notify_admin_backend();
CREATE TRIGGER admin_grants_notify_admin AFTER INSERT OR UPDATE OR DELETE ON admin_grants
  FOR EACH ROW EXECUTE FUNCTION rs_notify_admin('admins', '');
