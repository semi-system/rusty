-- Sieve scripts (docs/design-sieve.md): the filters a person writes for their incoming mail. An account has up to ten scripts (enforced in the repository) and at most one of them active; the delivering service
-- runs the active one on every message it stores for the account. The text is kept as it was uploaded, after the parser accepted it.
CREATE TABLE sieve_scripts (
  account_id bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  name       text COLLATE "C" NOT NULL CONSTRAINT sieve_scripts_name_valid CHECK (length(name) BETWEEN 1 AND 128 AND name !~ '[[:cntrl:]]'),
  body       text COLLATE "C" NOT NULL CONSTRAINT sieve_scripts_body_valid CHECK (length(body) <= 65536),
  active     boolean NOT NULL DEFAULT false,
  updated_at bigint NOT NULL,
  PRIMARY KEY (account_id, name)
);
-- One active script per account.
CREATE UNIQUE INDEX sieve_scripts_one_active ON sieve_scripts (account_id) WHERE active;
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (57, 56);
