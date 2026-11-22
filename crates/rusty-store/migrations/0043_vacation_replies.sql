-- Who an automatic reply (vacation) has already been sent to (docs/design-mail-rules.md): one row per account and sender, with the time of the last reply, so that a sender is answered once
-- in a week however much it writes. The row goes with its account; the garbage collector removes rows older than 60 days.
CREATE TABLE vacation_replies (
  account_id bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  sender     text COLLATE "C" NOT NULL CONSTRAINT vacation_replies_sender_valid CHECK (length(sender) BETWEEN 1 AND 320),
  at         bigint NOT NULL CONSTRAINT vacation_replies_at_valid CHECK (at >= 0),
  PRIMARY KEY (account_id, sender)
);
-- The cleanup of old rows.
CREATE INDEX vacation_replies_at ON vacation_replies (at);
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (43, 42);
