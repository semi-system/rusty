-- The address registry (address_owners) is written only by these two trigger functions. They run
-- as their owner (the migrator role), so the runtime role needs no write privilege on the table
-- and cannot corrupt the registry directly (deploy/db/roles.sql, grants_pg.sql, docs/postgres-production.md).
-- search_path is pinned with pg_temp LAST: without that a temporary table named like a registry
-- table would shadow it inside a definer function. The runtime role also has no TEMP privilege.
ALTER FUNCTION rs_address_unique() SECURITY DEFINER SET search_path = pg_catalog, public, pg_temp;
ALTER FUNCTION rs_address_truncate() SECURITY DEFINER SET search_path = pg_catalog, public, pg_temp;
