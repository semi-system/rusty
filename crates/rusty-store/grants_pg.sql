-- Privilege matrix of the Rusty database (applied by `rustyctl db migrate` after the migrations,
-- as the migrator role; idempotent). The single place that says who may do what to which table.
-- Roles themselves and their settings come from deploy/db/roles.sql. A role that does not exist is
-- skipped here (the migrate command refuses to run without the roles unless told otherwise).
--
--   rusty_app  : SELECT/INSERT/UPDATE/DELETE on every table, never TRUNCATE/DDL/TRIGGER/REFERENCES.
--                address_owners (the cross-table address registry) is SELECT only: it is written
--                solely by the SECURITY DEFINER trigger functions rs_address_unique and
--                rs_address_truncate (migration 0016), which the migrator owns.
--                _sqlx_migrations is SELECT only (the daemon reads the schema version, never writes it).
--   rusty_ro   : SELECT on every table except accounts.password_hash.
DO $grants$
DECLARE
  cols text;
BEGIN
  IF to_regrole('rusty_app') IS NOT NULL THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM rusty_app;
    GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO rusty_app;
    REVOKE INSERT, UPDATE, DELETE ON address_owners FROM rusty_app;
    REVOKE INSERT, UPDATE, DELETE ON _sqlx_migrations FROM rusty_app;
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM rusty_app;
    GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO rusty_app;
  END IF;
  IF to_regrole('rusty_ro') IS NOT NULL THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM rusty_ro;
    GRANT SELECT ON ALL TABLES IN SCHEMA public TO rusty_ro;
    REVOKE SELECT ON accounts FROM rusty_ro;
    SELECT string_agg(quote_ident(column_name), ', ' ORDER BY ordinal_position) INTO cols
      FROM information_schema.columns
      WHERE table_schema = 'public' AND table_name = 'accounts' AND column_name <> 'password_hash';
    EXECUTE format('GRANT SELECT (%s) ON accounts TO rusty_ro', cols);
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM rusty_ro;
  END IF;
END
$grants$;
