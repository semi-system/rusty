#!/usr/bin/env bash
# Remove this run's test Postgres container, its tmpfs data and its URL file. Idempotent.
# Only touches the run named by RUSTY_PG_RUN_ID (default "dev"); other runs are left alone, and
# a container is only removed when it carries this run's ownership label.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
# shellcheck source=/dev/null
. scripts/pg-lib.sh
RUSTY_PG_PASSWORD=unused "${COMPOSE[@]}" down -v >/dev/null 2>&1 || true
owner="$(docker inspect -f '{{index .Config.Labels "rusty.test-pg-run"}}' "$PG_NAME" 2>/dev/null || true)"
if [ -n "$owner" ] && [ "$owner" = "$RUN_ID" ]; then
  docker rm -f "$PG_NAME" >/dev/null 2>&1 || true
fi
rm -f "$URL_FILE"
echo "pg-down: done ($PG_NAME)"
exit 0
