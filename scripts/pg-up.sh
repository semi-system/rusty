#!/usr/bin/env bash
# Start the throw-away test Postgres (deploy/dev/postgres.compose.yaml). Run inside WSL/Linux
# with Docker. Writes the admin connection URL to $HOME/.rusty-pg-test-url.<run id> (mode 0600) and
# prints only a success line. Idempotent per run id (RUSTY_PG_RUN_ID, default "dev"): if that
# run's container is already running it is reused. Other runs' containers are never touched.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
umask 077
# shellcheck source=/dev/null
. scripts/pg-lib.sh
IMAGE_REF="$(awk '$1 == "postgres:18" { print $2 }' deploy/images.lock)"

cleanup() {
  RUSTY_PG_PASSWORD=unused "${COMPOSE[@]}" down -v >/dev/null 2>&1 || true
  docker rm -f "$PG_NAME" >/dev/null 2>&1 || true
  rm -f "$URL_FILE"
}

verify_server() {
  local version image
  version="$(docker exec "$PG_NAME" psql -h 127.0.0.1 -U rusty -d postgres -Atqc 'SHOW server_version_num' 2>/dev/null || true)"
  image="$(docker inspect -f '{{.Config.Image}}' "$PG_NAME" 2>/dev/null || true)"
  [[ "$version" =~ ^18[0-9]{4}$ ]] && [ "$image" = "$IMAGE_REF" ]
}

if [ "$(docker inspect -f '{{.State.Running}}' "$PG_NAME" 2>/dev/null || true)" = "true" ] && [ -s "$URL_FILE" ] && verify_server; then
  echo "pg-up: already running; URL in $URL_FILE"
  exit 0
fi

# Stale container or stale URL file: start clean.
cleanup

PASSWORD="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
[ "${#PASSWORD}" -eq 48 ] || { echo "pg-up: FAILED to generate a password" >&2; exit 1; }

export RUSTY_PG_PASSWORD="$PASSWORD"
if ! "${COMPOSE[@]}" up -d --wait --wait-timeout 90 >/dev/null 2>&1; then
  echo "pg-up: FAILED to start (inspect: docker logs $PG_NAME)" >&2
  cleanup
  exit 1
fi
if ! verify_server; then
  echo "pg-up: FAILED image/version verification" >&2
  cleanup
  exit 1
fi
MAPPING="$("${COMPOSE[@]}" port postgres 5432 2>/dev/null || true)"
PORT="${MAPPING##*:}"
if ! [[ "$PORT" =~ ^[0-9]+$ ]]; then
  echo "pg-up: FAILED to discover the loopback port" >&2
  cleanup
  exit 1
fi
# sslmode is explicit on purpose: the test container is loopback-only and has no TLS.
printf 'postgres://rusty:%s@127.0.0.1:%s/postgres?sslmode=disable\n' "$PASSWORD" "$PORT" > "$URL_FILE"
chmod 600 "$URL_FILE"
echo "pg-up: PostgreSQL ready on 127.0.0.1:$PORT; URL in $URL_FILE"
