-- Twin of SQLite migration 0013_jobs_owner_dedupe.sql (PG numbering equals the SQLite number).
ALTER TABLE jobs ADD COLUMN dedupe_key text CONSTRAINT jobs_dedupe_key_valid
  CHECK (dedupe_key IS NULL OR char_length(dedupe_key) BETWEEN 1 AND 128);
ALTER TABLE jobs ADD COLUMN owner_id bigint;
ALTER TABLE jobs ADD COLUMN units bigint NOT NULL DEFAULT 0 CONSTRAINT jobs_units_valid CHECK (units >= 0);
ALTER TABLE jobs ADD COLUMN bytes bigint NOT NULL DEFAULT 0 CONSTRAINT jobs_bytes_valid CHECK (bytes >= 0);
CREATE UNIQUE INDEX jobs_dedupe_key ON jobs (dedupe_key) WHERE dedupe_key IS NOT NULL;
CREATE INDEX jobs_owner_created ON jobs (owner_id, created_at) WHERE owner_id IS NOT NULL;
