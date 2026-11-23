-- What the deployment tool did about backups (docs/design-backup-status.md): one row per backup made, per drill (a dump restored into a scratch database and read), and per restore. Written by the tool with
-- the database login that stays the operator's (it has no part in the services), read by the admin web's Backups page, removed by the garbage collector after 400 days.
CREATE TABLE backup_runs (
  id     bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  at     bigint NOT NULL DEFAULT rs_now() CONSTRAINT backup_runs_at_valid CHECK (at >= 0),
  kind   text COLLATE "C" NOT NULL CONSTRAINT backup_runs_kind_valid CHECK (kind IN ('backup', 'verify', 'restore')),
  ok     boolean NOT NULL,
  -- The size of the dump for a backup; null for the others.
  bytes  bigint CONSTRAINT backup_runs_bytes_valid CHECK (bytes IS NULL OR bytes >= 0),
  -- A short line: what was checked or what failed (the counts of a drill, the reason of a failure).
  detail text COLLATE "C" NOT NULL DEFAULT '' CONSTRAINT backup_runs_detail_valid CHECK (length(detail) <= 500)
);
CREATE INDEX backup_runs_at ON backup_runs (at);
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (48, 47);
