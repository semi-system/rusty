#!/bin/sh
# One-shot, as the postgres image's psql: creates the roles (deploy/db/roles.sql) and gives the three that log in (app, admin, migrator) their passwords. Safe to run again.
set -eu
export PGHOST=db PGUSER=postgres PGDATABASE=rusty PGSSLMODE=verify-full PGSSLROOTCERT=/etc/rusty/db-ca.pem
PGPASSWORD="$(cat /run/secrets/pg_postgres)"; export PGPASSWORD
psql -v ON_ERROR_STOP=1 -q -f /roles.sql
psql -v ON_ERROR_STOP=1 -q -v app="$(cat /run/secrets/pg_app)" -v adm="$(cat /run/secrets/pg_admin)" -v mig="$(cat /run/secrets/pg_migrator)" <<'SQL'
ALTER ROLE rusty_app LOGIN PASSWORD :'app';
ALTER ROLE rusty_admin LOGIN PASSWORD :'adm';
ALTER ROLE rusty_migrator LOGIN PASSWORD :'mig';
SQL
echo "db-init: roles ready"
