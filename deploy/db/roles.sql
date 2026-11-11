-- Database roles of a Rusty installation (docs/postgres-production.md has the threat model).
--
-- Run ONCE per database, as a superuser (or a role that may CREATE ROLE and owns the database),
-- connected to the Rusty database, BEFORE the first `rustyctl db migrate`:
--     psql -v ON_ERROR_STOP=1 -d rusty -f deploy/db/roles.sql
-- The file is idempotent and plain SQL (no psql meta-commands, so a test harness can run it too).
-- It holds no password: the roles are created NOLOGIN. The operator gives each role that has to
-- log in a password (or a certificate) on purpose:
--     ALTER ROLE rusty_migrator LOGIN PASSWORD '...';   -- used only by `rustyctl db migrate`
--     ALTER ROLE rusty_app      LOGIN PASSWORD '...';   -- the daemon and rustyctl
--     ALTER ROLE rusty_ro       LOGIN PASSWORD '...';   -- optional, reporting
--
--   rusty_migrator  owns the schema and every object in it; the only role with DDL rights.
--   rusty_app       DML only (grants_pg.sql, applied by `rustyctl db migrate`): no DDL, no TRUNCATE,
--                   no write on the address registry, no TEMP, no CREATE on schema public.
--   rusty_ro        SELECT only, without accounts.password_hash.
--
-- Timeouts are properties of the role (ALTER ROLE .. SET), not of the session: the application
-- never issues SET on a connection (PgBouncer in transaction mode keeps working). The daemon
-- refuses to start when rusty_app has no statement_timeout / idle_in_transaction_session_timeout
-- or a longer one than [store] allows. lock_timeout is set by the application per transaction.

DO $roles$
BEGIN
  IF to_regrole('rusty_migrator') IS NULL THEN
    BEGIN
      CREATE ROLE rusty_migrator NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS INHERIT;
    EXCEPTION WHEN duplicate_object THEN NULL;
    END;
  END IF;
  IF to_regrole('rusty_app') IS NULL THEN
    BEGIN
      CREATE ROLE rusty_app NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS INHERIT;
    EXCEPTION WHEN duplicate_object THEN NULL;
    END;
  END IF;
  IF to_regrole('rusty_ro') IS NULL THEN
    BEGIN
      CREATE ROLE rusty_ro NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS INHERIT;
    EXCEPTION WHEN duplicate_object THEN NULL;
    END;
  END IF;
END
$roles$;

-- Role-level timeouts. The migrator runs long DDL; the runtime roles run short statements.
ALTER ROLE rusty_app SET statement_timeout = '30s';
ALTER ROLE rusty_app SET idle_in_transaction_session_timeout = '30s';
ALTER ROLE rusty_ro SET statement_timeout = '60s';
ALTER ROLE rusty_ro SET idle_in_transaction_session_timeout = '30s';
ALTER ROLE rusty_migrator SET statement_timeout = '15min';
ALTER ROLE rusty_migrator SET lock_timeout = '30s';
ALTER ROLE rusty_migrator SET idle_in_transaction_session_timeout = '5min';

-- Database-level access: nobody but the three roles (and superusers) connects; nobody creates
-- temporary tables except the migrator (a temporary table can shadow a real one for the session).
DO $db$
BEGIN
  EXECUTE format('REVOKE ALL ON DATABASE %I FROM PUBLIC', current_database());
  EXECUTE format('GRANT CONNECT ON DATABASE %I TO rusty_migrator, rusty_app, rusty_ro', current_database());
  EXECUTE format('GRANT TEMPORARY ON DATABASE %I TO rusty_migrator', current_database());
END
$db$;

-- Schema-level access: only the migrator creates objects in public.
REVOKE ALL ON SCHEMA public FROM PUBLIC;
GRANT USAGE, CREATE ON SCHEMA public TO rusty_migrator;
GRANT USAGE ON SCHEMA public TO rusty_app, rusty_ro;
