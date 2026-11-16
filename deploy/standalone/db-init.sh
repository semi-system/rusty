#!/bin/sh
# One-shot, as the postgres image's psql: creates the roles (deploy/db/roles.sql) and gives the three that log in (app, admin, migrator) their passwords. Safe to run again.
set -eu
export PGHOST=db PGUSER=postgres PGDATABASE=rusty PGSSLMODE=verify-full PGSSLROOTCERT=/etc/rusty/db-ca.pem
PGPASSWORD="$(cat /run/secrets/pg_postgres)"; export PGPASSWORD
psql -v ON_ERROR_STOP=1 -q -f /roles.sql
psql -v ON_ERROR_STOP=1 -q -v app="$(cat /run/secrets/pg_app)" -v adm="$(cat /run/secrets/pg_admin)" -v mig="$(cat /run/secrets/pg_migrator)" \
  -v smtp_in="$(cat /run/secrets/pg_smtp_in)" -v submission="$(cat /run/secrets/pg_submission)" -v imap="$(cat /run/secrets/pg_imap)" \
  -v outbound="$(cat /run/secrets/pg_outbound)" -v gc="$(cat /run/secrets/pg_gc)" <<'SQL'
ALTER ROLE rusty_app LOGIN PASSWORD :'app';
ALTER ROLE rusty_admin LOGIN PASSWORD :'adm';
ALTER ROLE rusty_migrator LOGIN PASSWORD :'mig';
ALTER ROLE rusty_smtp_in LOGIN PASSWORD :'smtp_in';
ALTER ROLE rusty_submission LOGIN PASSWORD :'submission';
ALTER ROLE rusty_imap LOGIN PASSWORD :'imap';
ALTER ROLE rusty_outbound LOGIN PASSWORD :'outbound';
ALTER ROLE rusty_gc LOGIN PASSWORD :'gc';
SQL
echo "db-init: roles ready"
