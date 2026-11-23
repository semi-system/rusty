-- What a person sends from the webmail (docs/design-webmail.md, sending). The webmail service parses hostile content and must not deliver mail to other accounts itself, so it writes the composed message
-- to the blob store and a row here; the submission service takes the row and sends the message through the very same path as an authenticated SMTP submission (the From check, the sending budget, the
-- local copies, the outbound queue) and then keeps a copy in the Sent folder. The same rows are the outbox the person sees.
CREATE TABLE webmail_outbox (
  id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  account_id  bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  -- The message as composed (headers and body), in the blob store of the platform.
  -- The address the person sent from (the envelope sender); the sender service checks that it is theirs.
  sender      text COLLATE "C" NOT NULL CONSTRAINT webmail_outbox_sender_valid CHECK (length(sender) <= 320),
  blob_hash   text COLLATE "C" NOT NULL CONSTRAINT webmail_outbox_blob_valid CHECK (length(blob_hash) = 64),
  size        bigint NOT NULL CONSTRAINT webmail_outbox_size_valid CHECK (size > 0),
  -- The envelope recipients (to, cc and bcc together) as a JSON array of addresses.
  recipients  text COLLATE "C" NOT NULL CONSTRAINT webmail_outbox_rcpts_valid CHECK (length(recipients) <= 40000),
  -- A short copy of the subject for the list; the message itself stays in the blob store.
  subject     text COLLATE "C" NOT NULL DEFAULT '' CONSTRAINT webmail_outbox_subject_valid CHECK (length(subject) <= 200),
  -- `queued`, `sending`, `sent` or `failed`.
  state       text COLLATE "C" NOT NULL DEFAULT 'queued' CONSTRAINT webmail_outbox_state_valid CHECK (state IN ('queued', 'sending', 'sent', 'failed')),
  -- A fixed word of why it failed (`not_yours`, `limit`, `refused`, `failed`), never text from a server.
  error       text COLLATE "C" CONSTRAINT webmail_outbox_error_valid CHECK (error IS NULL OR length(error) <= 32),
  created_at  bigint NOT NULL,
  finished_at bigint
);
CREATE INDEX webmail_outbox_queued ON webmail_outbox (id) WHERE state = 'queued';
CREATE INDEX webmail_outbox_account ON webmail_outbox (account_id, id DESC);
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (51, 50);
