#!/usr/bin/env bash
# Remove this run's test PgBouncer and its files. Idempotent; only the run named by RUSTY_PG_RUN_ID (default "dev") is touched.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
# shellcheck source=/dev/null
. scripts/pg-lib.sh
docker rm -f "rusty-pgb-test-$RUN_ID" >/dev/null 2>&1 || true
rm -rf "$HOME/.rusty-pgb-$RUN_ID" "$HOME/.rusty-pgb-test.$RUN_ID"
echo "pgb-down: done (rusty-pgb-test-$RUN_ID)"
exit 0
