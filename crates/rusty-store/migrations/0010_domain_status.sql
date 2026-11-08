-- A domain can be switched off without deleting anything (rustyctl domain disable): its
-- addresses stop resolving and its accounts stop authenticating. See repo/directory.rs.
ALTER TABLE domains ADD COLUMN status TEXT NOT NULL DEFAULT 'active'
  CHECK (status IN ('active','disabled'));
