-- Tenant rule (review I4): an alias and a list member must belong to the domain they are used in.
-- Enforced by the database so that no caller (facade, admin API, script) can break it. Existing rows
-- are not touched; only new and changed rows are checked. A missing target account is left to the
-- foreign key (a Conflict), hence `<>` and not `IS NOT`.
CREATE TRIGGER aliases_same_domain_ins BEFORE INSERT ON aliases
BEGIN
  SELECT RAISE(ABORT, 'invalid: alias target is in another domain')
  WHERE (SELECT domain_id FROM accounts WHERE id = NEW.account_id) <> NEW.domain_id;
END;
CREATE TRIGGER aliases_same_domain_upd BEFORE UPDATE OF domain_id, account_id ON aliases
BEGIN
  SELECT RAISE(ABORT, 'invalid: alias target is in another domain')
  WHERE (SELECT domain_id FROM accounts WHERE id = NEW.account_id) <> NEW.domain_id;
END;
CREATE TRIGGER list_members_same_domain_ins BEFORE INSERT ON list_members
BEGIN
  SELECT RAISE(ABORT, 'invalid: list member is in another domain')
  WHERE NEW.account_id IS NOT NULL
    AND (SELECT domain_id FROM accounts WHERE id = NEW.account_id)
        <> (SELECT domain_id FROM lists WHERE id = NEW.list_id);
END;
CREATE TRIGGER list_members_same_domain_upd BEFORE UPDATE OF account_id, list_id ON list_members
BEGIN
  SELECT RAISE(ABORT, 'invalid: list member is in another domain')
  WHERE NEW.account_id IS NOT NULL
    AND (SELECT domain_id FROM accounts WHERE id = NEW.account_id)
        <> (SELECT domain_id FROM lists WHERE id = NEW.list_id);
END;
