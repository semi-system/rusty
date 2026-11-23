-- The move of a mailbox from another IMAP server into an account of this one (docs/design-imap-migration.md): asked for in the admin web, done where the mailboxes are (the IMAP service) by pulling the
-- mail from the other server over IMAP, with progress that the admin web shows. The password of the other server is a sealed secret (`sealed_secrets`: owner kind `migration`, owner id the id of this
-- row, purpose `password`): the admin service can seal it and cannot read it, the IMAP service opens it. The row is removed two days after it was asked for, and the admin service then removes the secret.
CREATE TABLE imap_migrations (
  id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  account_id       bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  -- The administrator who asked, as the address it signed in with.
  requested_by     text COLLATE "C" NOT NULL CONSTRAINT imap_migrations_by_valid CHECK (length(requested_by) <= 320),
  host             text COLLATE "C" NOT NULL CONSTRAINT imap_migrations_host_valid CHECK (length(host) BETWEEN 1 AND 253),
  port             integer NOT NULL CONSTRAINT imap_migrations_port_valid CHECK (port BETWEEN 1 AND 65535),
  -- `tls` (implicit, usually 993), `starttls` (usually 143) or `plain` (no encryption: the password and the mail cross the network in the clear).
  security         text COLLATE "C" NOT NULL CONSTRAINT imap_migrations_security_valid CHECK (security IN ('tls', 'starttls', 'plain')),
  remote_user      text COLLATE "C" NOT NULL CONSTRAINT imap_migrations_user_valid CHECK (length(remote_user) BETWEEN 1 AND 320),
  -- `preparing` (asked for, the password not yet sealed), `queued`, `running`, `done`, `failed` or `cancelled`.
  state            text COLLATE "C" NOT NULL DEFAULT 'preparing' CONSTRAINT imap_migrations_state_valid CHECK (state IN ('preparing', 'queued', 'running', 'done', 'failed', 'cancelled')),
  -- The administrator asked to stop: the worker looks at it between messages.
  cancel_requested boolean NOT NULL DEFAULT false,
  -- The folder of the other server being copied now, and how many folders there are and are done.
  folder           text COLLATE "C" CONSTRAINT imap_migrations_folder_valid CHECK (folder IS NULL OR length(folder) <= 200),
  folders_total    integer NOT NULL DEFAULT 0 CONSTRAINT imap_migrations_ftotal_valid CHECK (folders_total >= 0),
  folders_done     integer NOT NULL DEFAULT 0 CONSTRAINT imap_migrations_fdone_valid CHECK (folders_done >= 0),
  messages         bigint NOT NULL DEFAULT 0,
  skipped          bigint NOT NULL DEFAULT 0,
  bytes            bigint NOT NULL DEFAULT 0,
  -- A fixed word of what went wrong (`connect`, `tls`, `auth`, `protocol`, `timeout`, `quota`, `too_large`, `failed`), never text from the other server.
  error            text COLLATE "C" CONSTRAINT imap_migrations_error_valid CHECK (error IS NULL OR length(error) <= 32),
  created_at       bigint NOT NULL,
  -- The worker's last sign of life; a running migration that has none for a while was left by a worker that stopped.
  updated_at       bigint,
  finished_at      bigint,
  expires_at       bigint NOT NULL
);
CREATE INDEX imap_migrations_account ON imap_migrations (account_id, id DESC);
CREATE INDEX imap_migrations_queued ON imap_migrations (id) WHERE state = 'queued';
CREATE INDEX imap_migrations_expires ON imap_migrations (expires_at);
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (50, 49);
