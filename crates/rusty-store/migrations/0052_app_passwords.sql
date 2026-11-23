-- App passwords (docs/design-app-passwords.md): extra passwords of an account, one per device or program, that the mail protocols (IMAP, SMTP submission, POP3) accept besides the account's own password and that
-- the person can revoke one by one. The webmail and the admin web never accept them, so a password given to a phone cannot open the mailbox in a browser. Only a hash is stored; the password itself is shown once.
CREATE TABLE app_passwords (
  id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  account_id   bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  label        text COLLATE "C" NOT NULL CONSTRAINT app_passwords_label_valid CHECK (length(label) BETWEEN 1 AND 60),
  hash         text COLLATE "C" NOT NULL CONSTRAINT app_passwords_hash_valid CHECK (length(hash) <= 200),
  created_at   bigint NOT NULL,
  last_used_at bigint
);
CREATE INDEX app_passwords_account ON app_passwords (account_id, id);
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (52, 51);
