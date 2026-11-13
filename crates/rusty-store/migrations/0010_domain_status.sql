-- Twin of SQLite migration 0010_domain_status.sql. The PG history is consolidated in 0001, so
-- this file takes the SQLite number (the gap 0002..0009 is intentional: it keeps parallel
-- worktrees that add twins from colliding on a "next free number").
ALTER TABLE domains ADD COLUMN status text NOT NULL DEFAULT 'active'
  CONSTRAINT domains_status_valid CHECK (status IN ('active','disabled'));
