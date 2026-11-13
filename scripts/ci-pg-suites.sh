#!/usr/bin/env bash
# The Postgres test suites of scripts/ci.sh against the server in RUSTY_TEST_PG_URL (the GitHub
# workflow provides one as a service container; scripts/ci.sh starts its own with pg-up.sh).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
: "${RUSTY_TEST_PG_URL:?set RUSTY_TEST_PG_URL}"
export RUSTY_REQUIRE_PG=1
cargo test -p rusty-store --features postgres
cargo test -p rusty-directory -p rusty-imap --features postgres
cargo test -p rusty-store --features postgres,mutation-testing --test pg_dialect_mutations
cargo clippy -p rusty-store -p rusty-directory -p rusty-imap --features postgres --all-targets -- -D warnings
