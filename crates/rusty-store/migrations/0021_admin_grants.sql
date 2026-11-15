-- Who may use the admin web (admin web design, section 5). An administrator is an ordinary account (e-mail and password) that has a row here:
-- `domain_id` NULL is a platform administrator (every domain, may create domains), a value is the administrator of that one domain only.
-- One grant per account: widening or narrowing a grant replaces it.
--
-- A domain administrator must be an account OF that domain (the trigger below, SQLSTATE RS002 like the other tenant rules), so a grant cannot
-- reach across tenants by construction. A platform administrator is an account of any domain. A grant goes with its account and with its domain.
-- Nothing here is a credential: the password is the account's own (Argon2, `accounts.password_hash`).
CREATE TABLE admin_grants (
  account_id bigint PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  domain_id  bigint REFERENCES domains(id) ON DELETE CASCADE,
  created_at bigint NOT NULL
);

-- The grants of one domain (the domain page lists its administrators) and the platform ones.
CREATE INDEX admin_grants_domain ON admin_grants (domain_id);

CREATE FUNCTION rs_admin_grant_same_domain() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
BEGIN
  IF NEW.domain_id IS NOT NULL AND EXISTS (SELECT 1 FROM accounts WHERE id = NEW.account_id AND domain_id <> NEW.domain_id) THEN
    RAISE EXCEPTION 'a domain administrator must be an account of that domain' USING ERRCODE = 'RS002';
  END IF;
  RETURN NEW;
END;
$$;
CREATE TRIGGER admin_grants_same_domain_ins BEFORE INSERT ON admin_grants
  FOR EACH ROW EXECUTE FUNCTION rs_admin_grant_same_domain();
CREATE TRIGGER admin_grants_same_domain_upd BEFORE UPDATE OF domain_id, account_id ON admin_grants
  FOR EACH ROW EXECUTE FUNCTION rs_admin_grant_same_domain();
