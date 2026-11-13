-- Outbound submissions (U5): an idempotency key, the submitting account and the work a job stands for.
--   dedupe_key: a second enqueue with the same key is a no-op that returns the existing job, so a
--               client retry after a temporary failure cannot create the message twice.
--   owner_id:   the account that submitted the job (no foreign key: jobs outlive account changes and
--               are only ever counted by owner).
--   units, bytes: recipients and message size, summed per owner over a window for the submission limit.
ALTER TABLE jobs ADD COLUMN dedupe_key TEXT CHECK (dedupe_key IS NULL OR length(dedupe_key) BETWEEN 1 AND 128);
ALTER TABLE jobs ADD COLUMN owner_id INTEGER;
ALTER TABLE jobs ADD COLUMN units INTEGER NOT NULL DEFAULT 0 CHECK (units >= 0);
ALTER TABLE jobs ADD COLUMN bytes INTEGER NOT NULL DEFAULT 0 CHECK (bytes >= 0);
CREATE UNIQUE INDEX jobs_dedupe_key ON jobs (dedupe_key) WHERE dedupe_key IS NOT NULL;
CREATE INDEX jobs_owner_created ON jobs (owner_id, created_at) WHERE owner_id IS NOT NULL;
