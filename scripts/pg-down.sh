#!/usr/bin/env bash
# Remove the test Postgres container and its tmpfs data, and the URL file. Idempotent.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
RUSTY_PG_PASSWORD=unused docker compose -f deploy/dev/postgres.compose.yaml down -v >/dev/null 2>&1 || true
docker rm -f rusty-pg-test >/dev/null 2>&1 || true
rm -f "$HOME/.rusty-pg-test-url"
echo "pg-down: done"
exit 0
