#!/usr/bin/env bash
# The database test suites run THROUGH a connection pooler (PgBouncer in transaction mode, scripts/pgb-up.sh) in front of the server in RUSTY_TEST_PG_URL: the code under test
# connects to the pooler while the fixtures (CREATE DATABASE, roles, migrations) stay direct. This is how a deployment behind a pooler runs, and what settles that prepared
# statements, `lock_timeout` per transaction and the rest work without session state (docs/postgres-production.md, "PgBouncer").
#
# The daemon tests are configured the way a deployment is (`pooler = true`, `direct_url`): the change feed's LISTEN goes to the database directly, everything else through the pooler.
# Skipped: the tests that open a store over the pooler URL alone and then wait for the feed (cross-process IDLE, the outbound worker's wake-up, a feed that cannot connect: they run
# direct in scripts/ci-pg-suites.sh), and the check of a role's timeouts (a pooled connection was opened before the test changed the role).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
: "${RUSTY_TEST_PG_URL:?set RUSTY_TEST_PG_URL}"
export RUSTY_REQUIRE_PG=1
export RUSTY_PG_RUN_ID="${RUSTY_PG_RUN_ID:-pgb}"
# shellcheck source=/dev/null
. scripts/pg-lib.sh
umask 077
printf '%s\n' "$RUSTY_TEST_PG_URL" > "$URL_FILE"
cleanup() { bash scripts/pgb-down.sh >/dev/null 2>&1 || true; rm -f "$URL_FILE"; }
trap cleanup EXIT
bash scripts/pgb-up.sh
RUSTY_TEST_PG_POOLED="$(cat "$HOME/.rusty-pgb-test.$RUN_ID")"
export RUSTY_TEST_PG_POOLED
cargo test --workspace -- --skip feed --skip startup_refuses_a_runtime_role
