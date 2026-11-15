-- Privilege matrix of the Rusty database (applied by `rustyctl db migrate` after the migrations,
-- as the migrator role; idempotent). The single place that says who may do what to which table.
-- Roles themselves and their settings come from deploy/db/roles.sql. A role that does not exist is
-- skipped here (the migrate command refuses to run without the roles unless told otherwise).
--
--   rusty_app  : SELECT/INSERT/UPDATE/DELETE on every table, never TRUNCATE/DDL/TRIGGER/REFERENCES.
--                address_owners (the cross-table address registry) is SELECT only: it is written
--                solely by the SECURITY DEFINER trigger functions rs_address_unique and
--                rs_address_truncate (migration 0016), which the migrator owns.
--                _sqlx_migrations is SELECT only (the daemon reads the schema version, never writes it).
--   rusty_admin: the admin web. DML on the tenant tables only (domains, accounts, aliases, lists, plans, capabilities, DKIM and DNS state, relays,
--                admin grants, jobs, the mailbox rows of an account, and the system folders it is made with); no access to message content, keywords, blob hashes, deliveries or the change log (two single columns, `blobs.store_id` and `messages.mailbox_id`, say only that a store or a domain holds something; `blob_stores` is its to manage). Sealed secrets:
--                INSERT, UPDATE, DELETE and SELECT of every column except `sealed` (the ciphertext): it can write a secret and never read one back.
--   rusty_ro   : SELECT on every table except accounts.password_hash and sealed_secrets (sealed ciphertext).
DO $grants$
DECLARE
  cols text;
BEGIN
  IF to_regrole('rusty_app') IS NOT NULL THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM rusty_app;
    GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO rusty_app;
    REVOKE INSERT, UPDATE, DELETE ON address_owners FROM rusty_app;
    REVOKE INSERT, UPDATE, DELETE ON _sqlx_migrations FROM rusty_app;
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM rusty_app;
    GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO rusty_app;
  END IF;
  IF to_regrole('rusty_admin') IS NOT NULL THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM rusty_admin;
    GRANT SELECT, INSERT, UPDATE, DELETE ON domains, accounts, aliases, lists, list_members, plans, capabilities, mailboxes,
      domain_dkim_keys, domain_dns_config, domain_dns_event, domain_dns_state, domain_relays, admin_grants, jobs TO rusty_admin;
    GRANT SELECT ON address_owners, _sqlx_migrations TO rusty_admin;
    GRANT SELECT, INSERT ON folders TO rusty_admin;   -- an account is made with its system folders
    -- Blob stores of domains (docs/design-s3-per-tenant.md): the settings, never a body. Two single columns are enough to know that a store or a domain already holds something:
    -- a store that holds a blob row, a domain whose mailboxes hold a message.
    GRANT SELECT, INSERT, UPDATE ON blob_stores TO rusty_admin;
    GRANT SELECT (store_id) ON blobs TO rusty_admin;
    GRANT SELECT (mailbox_id) ON messages TO rusty_admin;
    -- The backends of a pool are settings too; of where the objects are, the admin may know only which backend holds any (to refuse changing or removing one that does).
    GRANT SELECT, INSERT, UPDATE, DELETE ON blob_backends TO rusty_admin;
    GRANT SELECT (backend_id) ON blob_locations TO rusty_admin;
    GRANT INSERT, UPDATE, DELETE ON sealed_secrets TO rusty_admin;
    SELECT string_agg(quote_ident(column_name), ', ' ORDER BY ordinal_position) INTO cols
      FROM information_schema.columns
      WHERE table_schema = 'public' AND table_name = 'sealed_secrets' AND column_name <> 'sealed';
    EXECUTE format('GRANT SELECT (%s) ON sealed_secrets TO rusty_admin', cols);
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM rusty_admin;
    GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO rusty_admin;
  END IF;
  IF to_regrole('rusty_ro') IS NOT NULL THEN

    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM rusty_ro;
    GRANT SELECT ON ALL TABLES IN SCHEMA public TO rusty_ro;
    REVOKE SELECT ON accounts FROM rusty_ro;
    REVOKE SELECT ON sealed_secrets FROM rusty_ro;
    SELECT string_agg(quote_ident(column_name), ', ' ORDER BY ordinal_position) INTO cols
      FROM information_schema.columns
      WHERE table_schema = 'public' AND table_name = 'accounts' AND column_name <> 'password_hash';
    EXECUTE format('GRANT SELECT (%s) ON accounts TO rusty_ro', cols);
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM rusty_ro;
  END IF;
END
$grants$;
