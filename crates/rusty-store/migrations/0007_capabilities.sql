-- Hashed, revocable, short-lived capability tokens (rusty-directory `caps`). Only
-- SHA-256(tag || token) is stored: the bearer token itself never reaches the database.
--
-- SQLite cannot ALTER TABLE ADD CONSTRAINT, so UNIQUE (id, domain_id) on accounts is a unique
-- index; a composite foreign key may reference a unique index. The composite key makes
-- the denormalized domain_id column provably the account's own domain.
CREATE UNIQUE INDEX accounts_id_domain ON accounts (id, domain_id);

CREATE TABLE capabilities (
  token_hash  BLOB    NOT NULL PRIMARY KEY CHECK (typeof(token_hash) = 'blob' AND length(token_hash) = 32),
  account_id  INTEGER NOT NULL,
  domain_id   INTEGER NOT NULL,
  role        TEXT    NOT NULL CHECK (role IN ('imap','jmap','dav','smtp-submit')),
  issued_at   INTEGER NOT NULL,
  expires_at  INTEGER NOT NULL,
  revoked_at  INTEGER CHECK (revoked_at IS NULL OR revoked_at >= issued_at),
  CHECK (expires_at > issued_at AND expires_at - issued_at <= 86400),
  FOREIGN KEY (account_id, domain_id) REFERENCES accounts (id, domain_id) ON DELETE CASCADE
) STRICT;

-- Per-account active count / oldest-first revocation and revoke-all.
CREATE INDEX capabilities_account ON capabilities (account_id, issued_at);
-- Purge scans: expired rows and long-revoked rows.
CREATE INDEX capabilities_expires ON capabilities (expires_at);
CREATE INDEX capabilities_revoked ON capabilities (revoked_at) WHERE revoked_at IS NOT NULL;
