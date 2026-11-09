-- Twin of SQLite migration 0011_tenant_rules.sql (PG numbering equals the SQLite number, see 0010).
-- Same rule, SQLSTATE RS002 (mapped to Invalid).
CREATE FUNCTION rs_alias_same_domain() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
BEGIN
  IF EXISTS (SELECT 1 FROM accounts WHERE id = NEW.account_id AND domain_id <> NEW.domain_id) THEN
    RAISE EXCEPTION 'alias target is in another domain' USING ERRCODE = 'RS002';
  END IF;
  RETURN NEW;
END;
$$;
CREATE TRIGGER aliases_same_domain_ins BEFORE INSERT ON aliases
  FOR EACH ROW EXECUTE FUNCTION rs_alias_same_domain();
CREATE TRIGGER aliases_same_domain_upd BEFORE UPDATE OF domain_id, account_id ON aliases
  FOR EACH ROW EXECUTE FUNCTION rs_alias_same_domain();

CREATE FUNCTION rs_list_member_same_domain() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
BEGIN
  IF NEW.account_id IS NOT NULL AND EXISTS (
       SELECT 1 FROM accounts a JOIN lists l ON l.id = NEW.list_id
       WHERE a.id = NEW.account_id AND a.domain_id <> l.domain_id) THEN
    RAISE EXCEPTION 'list member is in another domain' USING ERRCODE = 'RS002';
  END IF;
  RETURN NEW;
END;
$$;
CREATE TRIGGER list_members_same_domain_ins BEFORE INSERT ON list_members
  FOR EACH ROW EXECUTE FUNCTION rs_list_member_same_domain();
CREATE TRIGGER list_members_same_domain_upd BEFORE UPDATE OF account_id, list_id ON list_members
  FOR EACH ROW EXECUTE FUNCTION rs_list_member_same_domain();
