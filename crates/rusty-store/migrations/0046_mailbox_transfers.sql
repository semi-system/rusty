-- Exports and imports of one mailbox, asked for in the admin web (docs/design-mailbox-transfers.md). The admin service has no access to mail, so a transfer goes through the database in pieces:
-- an export is written by the service that holds the mailboxes (the IMAP service) into chunks that the admin service streams to the browser; an import is uploaded to chunks by the admin service
-- (a request body is small) and read by the IMAP service. Everything is removed 24 hours after it was asked for.
CREATE TABLE mailbox_transfers (
  id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  kind         text COLLATE "C" NOT NULL CONSTRAINT mailbox_transfers_kind_valid CHECK (kind IN ('export', 'import')),
  account_id   bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  -- The administrator who asked, as the address it signed in with.
  requested_by text COLLATE "C" NOT NULL CONSTRAINT mailbox_transfers_by_valid CHECK (length(requested_by) <= 320),
  -- `uploading` (an import whose file is still arriving), `queued`, `running`, `done` or `failed`.
  state        text COLLATE "C" NOT NULL CONSTRAINT mailbox_transfers_state_valid CHECK (state IN ('uploading', 'queued', 'running', 'done', 'failed')),
  -- The folder an import goes to when the file does not say (the Inbox when empty).
  folder       text COLLATE "C" CONSTRAINT mailbox_transfers_folder_valid CHECK (folder IS NULL OR length(folder) <= 200),
  size         bigint NOT NULL DEFAULT 0 CONSTRAINT mailbox_transfers_size_valid CHECK (size >= 0),
  messages     bigint NOT NULL DEFAULT 0,
  skipped      bigint NOT NULL DEFAULT 0,
  -- A fixed word of what went wrong (`quota`, `unreadable`, `too_large`, `failed`), never text from the file.
  error        text COLLATE "C" CONSTRAINT mailbox_transfers_error_valid CHECK (error IS NULL OR length(error) <= 32),
  created_at   bigint NOT NULL,
  finished_at  bigint,
  expires_at   bigint NOT NULL
);
CREATE INDEX mailbox_transfers_account ON mailbox_transfers (account_id, id DESC);
-- The worker's look for what is waiting, and the removal of what has expired.
CREATE INDEX mailbox_transfers_queued ON mailbox_transfers (id) WHERE state = 'queued';
CREATE INDEX mailbox_transfers_expires ON mailbox_transfers (expires_at);
CREATE TABLE mailbox_transfer_chunks (
  transfer_id bigint NOT NULL REFERENCES mailbox_transfers(id) ON DELETE CASCADE,
  seq         integer NOT NULL CONSTRAINT mailbox_transfer_chunks_seq_valid CHECK (seq >= 0),
  data        bytea NOT NULL,
  PRIMARY KEY (transfer_id, seq)
);
-- Additive: a binary of the previous schema never reads the tables.
INSERT INTO schema_compat (version, min_binary) VALUES (46, 45);
