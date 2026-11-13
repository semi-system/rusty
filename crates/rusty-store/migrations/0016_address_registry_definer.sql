-- Postgres-only change (SECURITY DEFINER on the address registry trigger functions, privileges).
-- SQLite has no roles; this empty migration keeps the version numbers of both backends equal
-- (docs/postgres-production.md, "Migration numbering").
SELECT 1;
