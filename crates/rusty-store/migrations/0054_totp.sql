-- The second factor of the webmail sign-in (docs/design-2fa.md): a TOTP secret sealed to the webmail's own key, and recovery codes (hashes). Written by the webmail service only; the protocol services read
-- whether the factor is on (`confirmed_at`) to refuse the account's own password on IMAP, POP3 and SMTP submission, where an app password is used instead.
CREATE TABLE totp (
  account_id   bigint PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  -- The secret, sealed (HPKE) to the webmail's key with the account as the context: the database cannot read it.
  sealed       bytea NOT NULL CONSTRAINT totp_sealed_valid CHECK (length(sealed) BETWEEN 1 AND 512),
  -- Null until the person proved they can make a code.
  confirmed_at bigint,
  -- The last step accepted: a code is never accepted twice.
  last_step    bigint,
  created_at   bigint NOT NULL
);
CREATE TABLE totp_recovery (
  id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  account_id bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  hash       text COLLATE "C" NOT NULL CONSTRAINT totp_recovery_hash_valid CHECK (length(hash) <= 200)
);
CREATE INDEX totp_recovery_account ON totp_recovery (account_id);
-- Additive: a binary of the previous schema never reads the tables.
INSERT INTO schema_compat (version, min_binary) VALUES (54, 53);
