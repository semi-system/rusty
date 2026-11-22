-- The delivery log of outgoing mail (docs/design-queue.md): one row per recipient outcome of an attempt, written by the outbound worker, read by the message trace of the admin web, removed by the
-- garbage collector after 30 days. It answers "what happened to my mail to bob@far.example": delivered (to which server), deferred (why), bounced, or given up.
CREATE TABLE mail_log (
  id        bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  at        bigint NOT NULL CONSTRAINT mail_log_at_valid CHECK (at >= 0),
  -- The tenant the message was sent for. The log goes with the domain.
  domain_id bigint NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  -- The queue job, for a look at the queue; there is no foreign key (jobs are removed long before the log).
  job_id    bigint,
  sender    text COLLATE "C" NOT NULL CONSTRAINT mail_log_sender_valid CHECK (length(sender) <= 320),
  recipient text COLLATE "C" NOT NULL CONSTRAINT mail_log_recipient_valid CHECK (length(recipient) BETWEEN 1 AND 320),
  event     text COLLATE "C" NOT NULL CONSTRAINT mail_log_event_valid CHECK (event IN ('delivered', 'deferred', 'bounced', 'expired')),
  -- The server that answered, the enhanced status code and the words of the answer (fixed length: never raw peer text beyond this).
  remote    text COLLATE "C" CONSTRAINT mail_log_remote_valid CHECK (remote IS NULL OR length(remote) <= 255),
  status    text COLLATE "C" CONSTRAINT mail_log_status_valid CHECK (status IS NULL OR length(status) <= 16),
  detail    text COLLATE "C" CONSTRAINT mail_log_detail_valid CHECK (detail IS NULL OR length(detail) <= 512)
);
-- The trace of a domain, newest first; the search by recipient inside it reads the rows of the domain.
CREATE INDEX mail_log_domain ON mail_log (domain_id, id DESC);
-- The cleanup of rows past their retention.
CREATE INDEX mail_log_at ON mail_log (at);
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (36, 35);
