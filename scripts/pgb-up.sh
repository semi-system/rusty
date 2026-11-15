#!/usr/bin/env bash
# Start a throw-away PgBouncer (transaction pooling) in front of the test Postgres of scripts/pg-up.sh, for running the test suites through a pooler
# (docs/postgres-production.md, "PgBouncer"). Run inside WSL/Linux with Docker, after pg-up.sh. Writes `host:port` of the pooler to $HOME/.rusty-pgb-test.<run id>
# (mode 0600): export it as RUSTY_TEST_PG_POOLED and the code under test connects through the pooler while the fixtures (CREATE DATABASE, migrations) stay direct.
# Idempotent per run id (RUSTY_PG_RUN_ID, default "dev").
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
umask 077
# shellcheck source=/dev/null
. scripts/pg-lib.sh
IMAGE_REF="$(awk '$1 == "edoburu/pgbouncer:latest" { print $2 }' deploy/images.lock)"
[ -n "$IMAGE_REF" ] || { echo "pgb-up: edoburu/pgbouncer is not pinned in deploy/images.lock" >&2; exit 1; }
PGB_NAME="rusty-pgb-test-$RUN_ID"
PGB_FILE="$HOME/.rusty-pgb-test.$RUN_ID"
PGB_DIR="$HOME/.rusty-pgb-$RUN_ID"
[ -s "$URL_FILE" ] || { echo "pgb-up: start the test Postgres first (scripts/pg-up.sh)" >&2; exit 1; }

if [ "$(docker inspect -f '{{.State.Running}}' "$PGB_NAME" 2>/dev/null || true)" = "true" ] && [ -s "$PGB_FILE" ]; then
  echo "pgb-up: already running; $(cat "$PGB_FILE") in $PGB_FILE"
  exit 0
fi
docker rm -f "$PGB_NAME" >/dev/null 2>&1 || true
rm -rf "$PGB_DIR" "$PGB_FILE"
mkdir -p "$PGB_DIR"

# postgres://rusty:PASSWORD@127.0.0.1:PORT/postgres?sslmode=disable
url="$(cat "$URL_FILE")"
rest="${url#postgres://}"
creds="${rest%%@*}"
hostport="${rest#*@}"
hostport="${hostport%%/*}"
user="${creds%%:*}"
password="${creds#*:}"
pg_host="${hostport%%:*}"
pg_port="${hostport##*:}"
port="$(( (RANDOM % 20000) + 30000 ))"

# Any database name is passed on (the tests make one per test); the clients' passwords are looked up on the server with the superuser of the fixtures, so every test role logs
# in with its own password. Transaction pooling, with prepared statements handled by the pooler itself (PgBouncer 1.21 and later).
cat > "$PGB_DIR/pgbouncer.ini" <<INI
[databases]
* = host=$pg_host port=$pg_port

[pgbouncer]
listen_addr = 127.0.0.1
listen_port = $port
auth_type = scram-sha-256
auth_file = /etc/pgbouncer/userlist.txt
auth_user = $user
auth_query = SELECT usename, passwd FROM pg_shadow WHERE usename = \$1
pool_mode = transaction
max_client_conn = 2000
default_pool_size = 16
server_idle_timeout = 5
max_prepared_statements = 200
server_tls_sslmode = disable
ignore_startup_parameters = extra_float_digits,options,application_name
admin_users = $user
INI
printf '"%s" "%s"\n' "$user" "$password" > "$PGB_DIR/userlist.txt"
chmod 644 "$PGB_DIR/pgbouncer.ini" "$PGB_DIR/userlist.txt"

docker run -d --name "$PGB_NAME" --network host --label "rusty.test-pgb-run=$RUN_ID" \
  -v "$PGB_DIR/pgbouncer.ini:/etc/pgbouncer/pgbouncer.ini:ro" -v "$PGB_DIR/userlist.txt:/etc/pgbouncer/userlist.txt:ro" \
  --entrypoint pgbouncer "edoburu/pgbouncer@${IMAGE_REF#*@}" /etc/pgbouncer/pgbouncer.ini >/dev/null
for _ in $(seq 1 30); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
    printf '127.0.0.1:%s\n' "$port" > "$PGB_FILE"
    echo "pgb-up: PgBouncer ready on 127.0.0.1:$port in front of $pg_host:$pg_port; address in $PGB_FILE"
    exit 0
  fi
  sleep 1
done
echo "pgb-up: FAILED to start (inspect: docker logs $PGB_NAME)" >&2
docker rm -f "$PGB_NAME" >/dev/null 2>&1 || true
exit 1
