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

-- The services (deploy/db/roles.sql): one role each, holding what that service does and nothing else. rusty_app (the operator's role) keeps its wide DML; these are narrower.
-- Rows are locked with SELECT .. FOR UPDATE, which needs UPDATE on at least one column: where a service only locks a row it does not change, it gets UPDATE on one harmless column.
-- `accounts.password_hash` goes only to the services that check a password (submission, imap); a sealed secret's ciphertext only to those that open one (every service that opens
-- message bodies, and outbound for DKIM keys and relay passwords).
DO $service_grants$
DECLARE
  r text;
  cols text;
BEGIN
  SELECT string_agg(quote_ident(column_name), ', ' ORDER BY ordinal_position) INTO cols
    FROM information_schema.columns
    WHERE table_schema = 'public' AND table_name = 'accounts' AND column_name <> 'password_hash';
  FOREACH r IN ARRAY ARRAY['rusty_smtp_in', 'rusty_submission', 'rusty_imap', 'rusty_outbound', 'rusty_gc'] LOOP
    IF to_regrole(r) IS NOT NULL THEN
      EXECUTE format('REVOKE ALL ON ALL TABLES IN SCHEMA public FROM %I', r);
      EXECUTE format('REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM %I', r);
      EXECUTE format('GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO %I', r);
      EXECUTE format('GRANT SELECT ON _sqlx_migrations TO %I', r);
    END IF;
  END LOOP;

  -- rusty_smtp_in: inbound delivery. Reads the directory (no password hashes), writes messages and their blobs, never deletes.
  IF to_regrole('rusty_smtp_in') IS NOT NULL THEN
    EXECUTE format('GRANT SELECT (%s) ON accounts TO rusty_smtp_in', cols);
    GRANT SELECT ON domains, aliases, lists, list_members, plans, address_owners, mailboxes, folders, deliveries, blob_stores, blob_backends, sealed_secrets TO rusty_smtp_in;
    GRANT SELECT, INSERT, UPDATE ON messages, blobs, blob_locations TO rusty_smtp_in;
    GRANT INSERT ON keywords, change_log, deliveries, jobs TO rusty_smtp_in;
    GRANT UPDATE ON mailboxes, folders TO rusty_smtp_in;
  END IF;

  -- rusty_submission: authenticated submission. Checks a password, stores the copy and queues the outbound job.
  IF to_regrole('rusty_submission') IS NOT NULL THEN
    GRANT SELECT ON accounts, domains, aliases, lists, list_members, plans, address_owners, mailboxes, folders, deliveries, blob_stores, blob_backends, sealed_secrets TO rusty_submission;
    GRANT SELECT, INSERT, UPDATE ON messages, blobs, blob_locations, jobs TO rusty_submission;
    GRANT INSERT ON keywords, change_log, deliveries TO rusty_submission;
    GRANT UPDATE ON mailboxes, folders TO rusty_submission;
    GRANT UPDATE (created_at) ON domains, accounts TO rusty_submission;   -- row locks
  END IF;

  -- rusty_imap: mailbox access. Checks a password, reads and changes the messages of a mailbox.
  IF to_regrole('rusty_imap') IS NOT NULL THEN
    GRANT SELECT ON accounts, domains, aliases, lists, list_members, plans, address_owners, mailboxes, folders, deliveries, blob_stores, blob_backends, sealed_secrets TO rusty_imap;
    GRANT SELECT, INSERT, UPDATE, DELETE ON messages TO rusty_imap;
    GRANT SELECT, INSERT, UPDATE ON blobs, blob_locations TO rusty_imap;
    GRANT SELECT, INSERT, DELETE ON keywords TO rusty_imap;
    GRANT SELECT, INSERT ON change_log TO rusty_imap;
    GRANT INSERT, UPDATE, DELETE ON folders TO rusty_imap;
    GRANT UPDATE ON mailboxes TO rusty_imap;
    GRANT INSERT ON deliveries TO rusty_imap;
  END IF;

  -- rusty_outbound: the delivery worker. Claims and finishes jobs, reads the message and the DKIM and relay settings, writes the DNS check state.
  IF to_regrole('rusty_outbound') IS NOT NULL THEN
    EXECUTE format('GRANT SELECT (%s) ON accounts TO rusty_outbound', cols);
    GRANT SELECT ON domains, aliases, lists, list_members, mailboxes, folders, messages, deliveries, blobs, blob_locations, blob_stores, blob_backends, sealed_secrets,
      domain_dkim_keys, domain_relays, domain_dns_config TO rusty_outbound;
    GRANT SELECT, INSERT, UPDATE, DELETE ON jobs, domain_dns_state, domain_dns_event TO rusty_outbound;
    GRANT UPDATE (created_at) ON domains, accounts TO rusty_outbound;   -- the row locks of the DNS check and of a delivery
    GRANT UPDATE (check_status, checked_at) ON domain_relays TO rusty_outbound;
    -- a bounce (DSN) is delivered to the local sender like any inbound message
    GRANT INSERT, UPDATE ON messages, blobs, blob_locations TO rusty_outbound;
    GRANT INSERT ON keywords, change_log, deliveries TO rusty_outbound;
    GRANT UPDATE ON mailboxes, folders TO rusty_outbound;
  END IF;

  -- rusty_gc: garbage collection, store upkeep and the periodic jobs. Deletes what is no longer referenced; never creates a message.
  IF to_regrole('rusty_gc') IS NOT NULL THEN
    EXECUTE format('GRANT SELECT (%s) ON accounts TO rusty_gc', cols);
    GRANT SELECT ON domains, mailboxes, folders, deliveries, sealed_secrets TO rusty_gc;
    GRANT SELECT ON domain_dns_state TO rusty_gc;   -- the periodic DNS check asks which domains are due...
    GRANT UPDATE (created_at) ON domains TO rusty_gc;   -- ...and locks the domain row it queues a check for
    GRANT SELECT, DELETE ON messages, keywords, change_log TO rusty_gc;
    GRANT SELECT, UPDATE, DELETE ON capabilities TO rusty_gc;
    GRANT SELECT, UPDATE, DELETE ON blobs, deliveries TO rusty_gc;
    GRANT SELECT, INSERT, UPDATE, DELETE ON blob_locations, blob_store_leases, jobs TO rusty_gc;
    GRANT SELECT, INSERT, UPDATE ON blob_stores, blob_backends TO rusty_gc;
  END IF;
END
$service_grants$;
