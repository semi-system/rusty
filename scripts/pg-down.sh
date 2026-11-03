#!/usr/bin/env bash
# Remove this run's test Postgres container, its tmpfs data and its URL file. Idempotent.
# Only touches the run named by RUSTY_PG_RUN_ID (default "dev"); other runs are left alone.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
# shellcheck source=/dev/null
. scripts/pg-lib.sh
RUSTY_PG_PASSWORD=unused "${COMPOSE[@]}" down -v >/dev/null 2>&1 || true
docker rm -f "$PG_NAME" >/dev/null 2>&1 || true
rm -f "$URL_FILE"
echo "pg-down: done ($PG_NAME)"
exit 0
