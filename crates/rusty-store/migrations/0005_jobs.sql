-- Job queue with leases and fencing (distributed-model spec section 5).
--
-- A worker claims a job by taking a lease: state 'running', lease_owner, lease_expires_at, and
-- a higher epoch (the fencing token). Only the holder of the current epoch may heartbeat,
-- complete or fail the job, and any side effect it commits is fenced on that epoch inside the
-- same transaction (jobs.rs). A job whose lease lapsed is claimable again, with a new epoch.
--
-- Times are unix seconds from the database clock. payload is JSON, at most 64 KiB (bytes).
-- kind is [a-z0-9._-], 1..64 bytes. attempts counts claims and never exceeds max_attempts
-- (<= 20): a job out of attempts becomes 'dead'. The lease columns are set exactly when the
-- state is 'running'. 'failed' is reserved (nothing writes it today) and carries no lease.
CREATE TABLE jobs (
  id               INTEGER PRIMARY KEY,
  kind             TEXT NOT NULL
                   CHECK (length(kind) BETWEEN 1 AND 64 AND kind NOT GLOB '*[^a-z0-9._-]*'),
  domain_id        INTEGER REFERENCES domains(id) ON DELETE RESTRICT,
  state            TEXT NOT NULL DEFAULT 'queued'
                   CHECK (state IN ('queued', 'running', 'done', 'failed', 'dead')),
  payload          TEXT NOT NULL
                   CHECK (json_valid(payload) AND length(CAST(payload AS BLOB)) <= 65536),
  attempts         INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  max_attempts     INTEGER NOT NULL DEFAULT 5 CHECK (max_attempts BETWEEN 1 AND 20),
  run_after        INTEGER NOT NULL,
  lease_owner      TEXT CHECK (lease_owner IS NULL OR length(lease_owner) BETWEEN 1 AND 128),
  lease_expires_at INTEGER,
  epoch            INTEGER NOT NULL DEFAULT 0 CHECK (epoch >= 0),
  last_error       TEXT CHECK (last_error IS NULL OR length(CAST(last_error AS BLOB)) <= 512),
  created_at       INTEGER NOT NULL,
  updated_at       INTEGER NOT NULL,
  CHECK (attempts <= max_attempts),
  CHECK ((state = 'running' AND lease_owner IS NOT NULL AND lease_expires_at IS NOT NULL)
      OR (state <> 'running' AND lease_owner IS NULL AND lease_expires_at IS NULL))
) STRICT;
CREATE INDEX jobs_state_run_after ON jobs (state, run_after);
