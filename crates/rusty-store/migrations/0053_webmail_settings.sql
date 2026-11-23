-- What a person sets in the webmail for themselves (docs/design-webmail.md): for now the signature that a new message starts with. One row per account, written by the webmail service only.
CREATE TABLE webmail_settings (
  account_id bigint PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  signature  text COLLATE "C" NOT NULL DEFAULT '' CONSTRAINT webmail_settings_signature_valid CHECK (length(signature) <= 2000),
  updated_at bigint NOT NULL
);
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (53, 52);
