-- Searches of the mail of every mailbox for a platform administrator (docs/design-admin-search.md). The admin service has no access to mail, so a search goes through the database like a transfer: the admin
-- service writes the request, the service that holds the mailboxes (the IMAP service) runs it and writes at most 100 results (who, which folder, when, from, subject, size: never a body), the admin service
-- reads them. Off unless the platform setting `admin_search` is on; every request is also written to the audit log. Everything is removed 24 hours after it was asked for.
CREATE TABLE message_searches (
  id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  requested_by text COLLATE "C" NOT NULL CONSTRAINT message_searches_by_valid CHECK (length(requested_by) <= 320),
  -- Narrow it: one domain, or one account (by address). Neither: every mailbox.
  domain       text COLLATE "C" CONSTRAINT message_searches_domain_valid CHECK (domain IS NULL OR length(domain) <= 253),
  account      text COLLATE "C" CONSTRAINT message_searches_account_valid CHECK (account IS NULL OR length(account) <= 320),
  -- Text the sender, a recipient or the subject contains (case does not matter); the service requires at least one of these or a date.
  from_text    text COLLATE "C" CONSTRAINT message_searches_from_valid CHECK (from_text IS NULL OR length(from_text) <= 200),
  to_text      text COLLATE "C" CONSTRAINT message_searches_to_valid CHECK (to_text IS NULL OR length(to_text) <= 200),
  subject_text text COLLATE "C" CONSTRAINT message_searches_subject_valid CHECK (subject_text IS NULL OR length(subject_text) <= 200),
  since        bigint,
  until        bigint,
  -- `queued`, `running`, `done` or `failed`.
  state        text COLLATE "C" NOT NULL DEFAULT 'queued' CONSTRAINT message_searches_state_valid CHECK (state IN ('queued', 'running', 'done', 'failed')),
  found        integer NOT NULL DEFAULT 0 CONSTRAINT message_searches_found_valid CHECK (found >= 0),
  -- More matched than the results that are kept.
  more_matched boolean NOT NULL DEFAULT false,
  -- The results as a JSON array of objects, written by the worker.
  results      text COLLATE "C" NOT NULL DEFAULT '[]' CONSTRAINT message_searches_results_valid CHECK (length(results) <= 200000),
  -- A fixed word (`timeout`, `failed`), never text from a mailbox.
  error        text COLLATE "C" CONSTRAINT message_searches_error_valid CHECK (error IS NULL OR length(error) <= 32),
  created_at   bigint NOT NULL,
  finished_at  bigint,
  expires_at   bigint NOT NULL
);
CREATE INDEX message_searches_queued ON message_searches (id) WHERE state = 'queued';
CREATE INDEX message_searches_expires ON message_searches (expires_at);
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (49, 48);
