#!/usr/bin/env bash
# The Postgres test suites of scripts/ci.sh against the server in RUSTY_TEST_PG_URL (the GitHub
# workflow provides one as a service container; scripts/ci.sh starts its own with pg-up.sh).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
: "${RUSTY_TEST_PG_URL:?set RUSTY_TEST_PG_URL}"
export RUSTY_REQUIRE_PG=1
# nextest runs the test binaries side by side (cargo test runs them one after the other): the same tests in a fraction of the time.
command -v cargo-nextest >/dev/null || { echo "cargo-nextest is needed: cargo install --locked cargo-nextest" >&2; exit 1; }
cargo nextest run -p rusty-store
cargo nextest run -p rusty-directory -p rusty-imap
cargo nextest run -p rusty-store --features mutation-testing --test pg_dialect_mutations
cargo clippy -p rusty-store -p rusty-directory -p rusty-imap --all-targets -- -D warnings
