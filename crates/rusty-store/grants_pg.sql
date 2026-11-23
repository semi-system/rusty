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
    GRANT EXECUTE ON FUNCTION rs_heartbeat(text, text, text, bigint, text, text) TO rusty_app;
  END IF;
  IF to_regrole('rusty_admin') IS NOT NULL THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM rusty_admin;
    GRANT SELECT ON mail_log, message_stats TO rusty_admin;
    GRANT SELECT, INSERT, UPDATE ON mailbox_transfers TO rusty_admin;   -- an export or import asked for in the admin web, and the pieces of the file (it never reads a mailbox itself)
    GRANT SELECT, INSERT ON mailbox_transfer_chunks TO rusty_admin;   -- the message trace
    GRANT SELECT, INSERT ON audit_log TO rusty_admin;   -- the audit log of the admin web (docs/design-audit.md)
    GRANT SELECT, INSERT, UPDATE ON imap_migrations TO rusty_admin;   -- the move of a mailbox from another IMAP server: asked for, watched and stopped here, done where the mailboxes are (docs/design-imap-migration.md)
    GRANT SELECT, INSERT ON message_searches TO rusty_admin;   -- a platform administrator's search is asked for here and answered where the mailboxes are (docs/design-admin-search.md)
    GRANT SELECT ON backup_runs TO rusty_admin;   -- the Backups page of the admin web (docs/design-backup-status.md); the deployment tool writes with its own login
    GRANT SELECT, INSERT, UPDATE, DELETE ON domains, accounts, aliases, domain_aliases, lists, list_members, plans, capabilities, mailboxes,
      domain_dkim_keys, dkim_retirements, domain_dns_config, domain_dns_event, domain_dns_state, domain_relays, admin_grants, jobs TO rusty_admin;
    GRANT SELECT ON address_owners, _sqlx_migrations, schema_compat TO rusty_admin;
    GRANT SELECT ON service_heartbeats TO rusty_admin;   -- the monitor page
    GRANT SELECT, INSERT, UPDATE ON platform_settings TO rusty_admin;   -- the protocol switches (docs/design-protocols.md)
    GRANT EXECUTE ON FUNCTION rs_heartbeat(text, text, text, bigint, text, text) TO rusty_admin;
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
  FOREACH r IN ARRAY ARRAY['rusty_smtp_in', 'rusty_submission', 'rusty_imap', 'rusty_outbound', 'rusty_gc', 'rusty_webmail'] LOOP
    IF to_regrole(r) IS NOT NULL THEN
      EXECUTE format('REVOKE ALL ON ALL TABLES IN SCHEMA public FROM %I', r);
      EXECUTE format('REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM %I', r);
      EXECUTE format('GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO %I', r);
      EXECUTE format('GRANT SELECT ON _sqlx_migrations, schema_compat TO %I', r);
      EXECUTE format('GRANT SELECT ON platform_settings TO %I', r);   -- the services read the protocol switches
      EXECUTE format('GRANT EXECUTE ON FUNCTION rs_heartbeat(text, text, text, bigint, text, text) TO %I', r);   -- the heartbeat of the service (docs/design-admin-monitor.md)
    END IF;
  END LOOP;

  -- rusty_smtp_in: inbound delivery. Reads the directory (no password hashes), writes messages and their blobs, never deletes.
  IF to_regrole('rusty_smtp_in') IS NOT NULL THEN
    EXECUTE format('GRANT SELECT (%s) ON accounts TO rusty_smtp_in', cols);
    GRANT SELECT ON domains, aliases, domain_aliases, lists, list_members, plans, address_owners, mailboxes, folders, deliveries, blob_stores, blob_backends, sealed_secrets TO rusty_smtp_in;
    GRANT SELECT, INSERT, UPDATE ON messages, blobs, blob_locations TO rusty_smtp_in;
    GRANT INSERT ON keywords, change_log, deliveries, jobs TO rusty_smtp_in;
    GRANT SELECT, INSERT, UPDATE ON vacation_replies TO rusty_smtp_in;   -- one automatic reply per sender and week (docs/design-mail-rules.md)
    GRANT INSERT ON mail_log TO rusty_smtp_in;   -- the trace of incoming mail (docs/design-queue.md)
    GRANT UPDATE ON mailboxes, folders TO rusty_smtp_in;
  END IF;

  -- rusty_submission: authenticated submission. Checks a password, stores the copy and queues the outbound job.
  IF to_regrole('rusty_submission') IS NOT NULL THEN
    GRANT SELECT ON accounts, domains, aliases, domain_aliases, lists, list_members, plans, address_owners, mailboxes, folders, deliveries, blob_stores, blob_backends, sealed_secrets TO rusty_submission;
    GRANT SELECT, UPDATE ON webmail_outbox TO rusty_submission;   -- what a person sent from the webmail is sent here, like a submission (docs/design-webmail.md)
    GRANT SELECT, INSERT, UPDATE ON messages, blobs, blob_locations, jobs TO rusty_submission;
    GRANT INSERT ON keywords, change_log, deliveries TO rusty_submission;
    GRANT UPDATE ON mailboxes, folders TO rusty_submission;
    GRANT UPDATE (created_at) ON domains, accounts TO rusty_submission;   -- row locks
    GRANT UPDATE (last_login_at) ON accounts TO rusty_submission;   -- the time of the last sign-in (DirectoryRepo::record_login)
  END IF;

  -- rusty_imap: mailbox access. Checks a password, reads and changes the messages of a mailbox.
  IF to_regrole('rusty_imap') IS NOT NULL THEN
    GRANT SELECT ON accounts, domains, aliases, domain_aliases, lists, list_members, plans, address_owners, mailboxes, folders, deliveries, blob_stores, blob_backends, sealed_secrets TO rusty_imap;
    GRANT UPDATE (last_login_at) ON accounts TO rusty_imap;   -- the time of the last sign-in (DirectoryRepo::record_login)
    GRANT SELECT, UPDATE ON mailbox_transfers TO rusty_imap;   -- the exports and imports of the admin web are done where the mailboxes are (docs/design-mailbox-transfers.md)
    GRANT SELECT, UPDATE ON imap_migrations TO rusty_imap;   -- the worker of the move from another IMAP server (docs/design-imap-migration.md)
    GRANT SELECT, UPDATE ON message_searches TO rusty_imap;   -- the searches of the admin web are run where the mailboxes are (docs/design-admin-search.md)
    GRANT SELECT, INSERT ON mailbox_transfer_chunks TO rusty_imap;
    GRANT SELECT, INSERT, UPDATE, DELETE ON messages TO rusty_imap;
    GRANT SELECT, INSERT, UPDATE ON blobs, blob_locations TO rusty_imap;
    GRANT SELECT, INSERT, DELETE ON keywords TO rusty_imap;
    GRANT SELECT, INSERT ON change_log TO rusty_imap;
    GRANT INSERT, UPDATE, DELETE ON folders TO rusty_imap;
    GRANT UPDATE ON mailboxes TO rusty_imap;
    GRANT INSERT ON deliveries TO rusty_imap;
  END IF;

  -- rusty_webmail: the people who own a mailbox read and write mail in a browser (docs/design-webmail.md). In the first step it signs people in and nothing else: the directory (with the password hashes it checks), the time of the last
  -- sign-in, and (read path) the messages and where their bodies are. What it needs to write mail is granted with the write path.
  IF to_regrole('rusty_webmail') IS NOT NULL THEN
    GRANT SELECT ON accounts, domains, aliases, domain_aliases, lists, list_members, plans, address_owners, mailboxes, folders TO rusty_webmail;
    GRANT UPDATE (last_login_at) ON accounts TO rusty_webmail;   -- the time of the last sign-in (DirectoryRepo::record_login)
    -- The read path: the messages and their bodies (never a write: nothing of the mailbox changes by reading it) and where a body is.
    GRANT SELECT ON messages, keywords, blobs, blob_locations, blob_stores, blob_backends, deliveries TO rusty_webmail;
    -- The write path: organising the mail (flags, move, copy, expunge, folders) like the mailbox access of rusty_imap does; a copy shares the body, so the blob rows are touched too. Never a delete of a blob: that is the collector's.
    GRANT INSERT, UPDATE, DELETE ON messages, folders TO rusty_webmail;
    GRANT INSERT, DELETE ON keywords TO rusty_webmail;
    GRANT INSERT, UPDATE ON blobs, blob_locations TO rusty_webmail;
    GRANT SELECT, INSERT ON change_log TO rusty_webmail;
    GRANT INSERT ON deliveries TO rusty_webmail;
    GRANT UPDATE ON mailboxes TO rusty_webmail;
    GRANT SELECT, INSERT, DELETE ON webmail_outbox TO rusty_webmail;   -- what a person sends is queued here and read back as the outbox; the message itself is a blob (docs/design-webmail.md)
  END IF;

  -- rusty_outbound: the delivery worker. Claims and finishes jobs, reads the message and the DKIM and relay settings, writes the DNS check state.
  IF to_regrole('rusty_outbound') IS NOT NULL THEN
    EXECUTE format('GRANT SELECT (%s) ON accounts TO rusty_outbound', cols);
    GRANT SELECT ON domains, aliases, domain_aliases, lists, list_members, mailboxes, folders, messages, deliveries, blobs, blob_locations, blob_stores, blob_backends, sealed_secrets,
      domain_dkim_keys, domain_relays, domain_dns_config TO rusty_outbound;
    GRANT SELECT, INSERT, UPDATE, DELETE ON jobs, domain_dns_state, domain_dns_event TO rusty_outbound;
    GRANT INSERT ON mail_log TO rusty_outbound;   -- the delivery log (docs/design-queue.md)
    GRANT SELECT, INSERT, UPDATE ON job_turns TO rusty_outbound;   -- the turn order of the tenants (docs/design-fair-claim.md)
    GRANT UPDATE (created_at) ON domains, accounts TO rusty_outbound;   -- the row locks of the DNS check and of a delivery
    GRANT UPDATE (check_status, checked_at) ON domain_relays TO rusty_outbound;
    GRANT UPDATE (check_status, checked_at) ON blob_stores, blob_backends TO rusty_outbound;
    GRANT UPDATE (used_bytes, promised_bytes, unlimited_accounts, capacity_status) ON blob_stores TO rusty_outbound;   -- the capacity verdict of the storage check (docs/design-storage-capacity.md)
    GRANT SELECT ON plans TO rusty_outbound;   -- ...which sums the quotas the plans give   -- the storage check worker locks the row, then records its result; a tenant's store is used only after a check passed
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
    GRANT DELETE ON accounts TO rusty_gc;   -- the removal of a closed account after its retention (the rows that hang on it go by cascade); locks it first
    GRANT UPDATE (created_at) ON accounts TO rusty_gc;   -- FOR UPDATE
    GRANT SELECT, UPDATE, DELETE ON capabilities TO rusty_gc;
    GRANT SELECT, UPDATE, DELETE ON blobs, deliveries TO rusty_gc;
    GRANT SELECT, INSERT, UPDATE, DELETE ON blob_locations, blob_store_leases, jobs TO rusty_gc;
    GRANT SELECT, INSERT, UPDATE ON job_turns TO rusty_gc;
    GRANT SELECT, DELETE ON mail_log TO rusty_gc;
    GRANT SELECT, DELETE ON mailbox_transfers, mailbox_transfer_chunks TO rusty_gc;   -- an export or import is kept a day (docs/design-mailbox-transfers.md)
    GRANT SELECT, INSERT, UPDATE, DELETE ON message_stats TO rusty_gc;   -- the roll-up of the delivery log into days (docs/design-queue.md)   -- the log is kept 30 days
    GRANT SELECT, DELETE ON audit_log TO rusty_gc;
    GRANT SELECT, DELETE ON imap_migrations TO rusty_gc;   -- a migration is kept two days (its sealed password is removed by the admin service)
    GRANT SELECT, DELETE ON message_searches TO rusty_gc;   -- a search is kept a day
    GRANT SELECT, DELETE ON webmail_outbox TO rusty_gc;   -- what was sent is kept a week
    GRANT SELECT, DELETE ON backup_runs TO rusty_gc;   -- kept 400 days
    GRANT SELECT, DELETE ON vacation_replies TO rusty_gc;   -- the vacation book-keeping is kept 60 days (docs/design-mail-rules.md)   -- the audit log is kept 365 days
    GRANT SELECT, INSERT, UPDATE ON blob_stores, blob_backends TO rusty_gc;
    GRANT SELECT, DELETE ON service_heartbeats TO rusty_gc;   -- the cleanup of heartbeats nobody renews
  END IF;
END
$service_grants$;
