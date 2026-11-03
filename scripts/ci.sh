#!/usr/bin/env bash
# Local CI gate. Run from anywhere: bash scripts/ci.sh
# Exits non-zero on the first failing stage. See SECURITY.md for tool installation.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

MUSL=x86_64-unknown-linux-musl
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
FUZZ_SECS="${FUZZ_SECS:-60}"
PG_SKIPPED=0
FUZZ_DEGRADED=0
STAGE="startup"
PG_RUN_ID=""

# One EXIT trap for the whole script, never cleared: it tears down this run's Postgres container
# (if one is up) and reports the stage that was active when the script exited non-zero, also for
# failures that happen outside stage() (set -e, missing tools, signals turned into exits).
on_exit() {
  local rc=$?
  trap - EXIT
  if [ -n "$PG_RUN_ID" ]; then
    RUSTY_PG_RUN_ID="$PG_RUN_ID" bash scripts/pg-down.sh >/dev/null 2>&1 || true
  fi
  if [ "$rc" -ne 0 ]; then
    echo "[FAIL] ci.sh exited with $rc during $STAGE" >&2
  fi
  exit "$rc"
}
trap on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}"

STAGES=0
stage() { # name, command...
  local name="$1"; shift
  STAGE="$name"
  echo "==> $name"
  # Test hook for scripts/tests/ci-trap.sh only: force this stage to fail.
  if [ "${RUSTY_CI_FORCE_FAIL:-}" = "$name" ]; then set -- false; fi
  if "$@"; then
    STAGES=$((STAGES + 1))
    echo "[ OK ] $name"
  else
    echo "[FAIL] $name" >&2
    exit 1
  fi
}

need() { command -v "$1" >/dev/null 2>&1 || { echo "[FAIL] missing tool: $1 (see SECURITY.md)" >&2; exit 1; }; }
need cargo; need "$CC_x86_64_unknown_linux_musl"; need file
cargo deny --version >/dev/null 2>&1 || { echo "[FAIL] missing tool: cargo-deny" >&2; exit 1; }
cargo audit --version >/dev/null 2>&1 || { echo "[FAIL] missing tool: cargo-audit" >&2; exit 1; }
rustup target list --installed | grep -qx "$MUSL" || { echo "[FAIL] missing rustup target $MUSL" >&2; exit 1; }

# The repo is not rustfmt-clean yet, so formatting is not gated here; clippy is.
# (skipped when the hook is set: ci-trap.sh runs ci.sh itself)
[ -n "${RUSTY_CI_FORCE_FAIL:-}" ] || stage "ci scripts" bash scripts/tests/ci-trap.sh

# Database access belongs to rusty-store: no crate outside it may use a raw pool or open a
# transaction (the directory sits on DirectoryRepo). Tests are exempt until PG6.
no_raw_pool_outside_store() {
  local hits
  hits="$(grep -rnE '\.pool\(\)|begin_write|SqliteTx' crates/*/src --include='*.rs' | grep -v '^crates/rusty-store/' || true)"
  if [ -n "$hits" ]; then
    echo "raw pool / transaction use outside rusty-store:" >&2
    echo "$hits" >&2
    return 1
  fi
}
stage "no raw pool outside store" no_raw_pool_outside_store
stage "clippy"       cargo clippy --workspace --all-targets -- -D warnings
stage "test (glibc)" cargo test --workspace

postgres_tests() {
  if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
    PG_SKIPPED=1
    echo "WARNING: Docker is unavailable; PostgreSQL smoke tests were not run." >&2
    return 0
  fi

  # A fresh run id per CI run: concurrent runs get their own container and URL file. on_exit
  # removes exactly this run's container, even if this stage dies half-way.
  RUSTY_PG_RUN_ID="ci$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
  export RUSTY_PG_RUN_ID
  PG_RUN_ID="$RUSTY_PG_RUN_ID"
  local url_file="$HOME/.rusty-pg-test-url.$RUSTY_PG_RUN_ID" url rc=0
  bash scripts/pg-up.sh || return 1
  if ! IFS= read -r url < "$url_file" || [ -z "$url" ]; then
    echo "PostgreSQL URL file is missing or empty" >&2
    return 1
  fi
  RUSTY_TEST_PG_URL="$url" RUSTY_REQUIRE_PG=1 \
    cargo test -p rusty-store --features postgres --test pg_smoke || rc=$?
  if [ "$rc" -eq 0 ]; then
    RUSTY_TEST_PG_URL="$url" RUSTY_REQUIRE_PG=1 \
      cargo test -p rusty-store --features postgres \
        --test pg_schema_constraints --test pg_schema_cases --test pg_schema_concurrency \
        --test pg_schema_parity --test spike_conformance --test spike_pg --test pg_guard \
        --test error_busy --test clock_policy || rc=$?
  fi
  if [ "$rc" -eq 0 ]; then
    RUSTY_TEST_PG_URL="$url" RUSTY_REQUIRE_PG=1 \
      cargo test -p rusty-directory --features postgres --test directory_conformance || rc=$?
  fi
  if [ "$rc" -eq 0 ]; then
    RUSTY_TEST_PG_URL="$url" RUSTY_REQUIRE_PG=1 \
      cargo clippy -p rusty-store -p rusty-directory --features postgres --all-targets -- -D warnings || rc=$?
  fi
  bash scripts/pg-down.sh >/dev/null 2>&1 || true
  PG_RUN_ID=""
  return "$rc"
}
stage "postgres" postgres_tests

# Two concurrent runs get isolated containers (needs Docker). Runs after clippy/test, so
# ci-trap.sh (which forces clippy to fail) never reaches it and cannot start it a second time.
pg_isolation() {
  if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
    PG_SKIPPED=1
    echo "WARNING: Docker is unavailable; pg isolation test was not run." >&2
    return 0
  fi
  bash scripts/tests/pg-isolation.sh
}
stage "pg isolation" pg_isolation
stage "cargo-deny"   cargo deny check
stage "cargo-audit"  cargo audit
stage "test (musl)"  cargo test --workspace --target "$MUSL"
stage "build (musl release)" cargo build --release --workspace --target "$MUSL"

check_static() {
  local found=0 f
  shopt -s nullglob
  for f in "$TARGET_DIR/$MUSL"/release/*; do
    [ -f "$f" ] && [ -x "$f" ] || continue
    case "$f" in *.d|*.rlib|*.so|*.a) continue ;; esac
    file -L "$f" | grep -q "ELF" || continue
    found=$((found + 1))
    if file -L "$f" | grep -qE "statically linked|static-pie linked"; then
      echo "static: $f"
    else
      echo "NOT static: $f" >&2; file -L "$f" >&2; return 1
    fi
  done
  echo "checked $found release binaries"
}
stage "static linking" check_static

fuzz_all() {
  if command -v cargo-fuzz >/dev/null 2>&1 && rustup toolchain list | grep -q "^nightly"; then
    local t
    for t in parse_envelope parse_address; do
      mkdir -p "fuzz/corpus/$t"
      # Working corpus first (new finds are written there), committed seeds second.
      (cd fuzz && cargo +nightly fuzz run "$t" "corpus/$t" "seeds/$t" -- "-max_total_time=$FUZZ_SECS" -rss_limit_mb=2048) || return 1
    done
  else
    FUZZ_DEGRADED=1
    echo "WARNING: cargo-fuzz and/or the nightly toolchain are missing; running the proptest fallback only (NOT real fuzzing)." >&2
    cargo test -p rusty-mime -p rusty-directory --test fuzz_smoke
  fi
}

stage "compose hardening check" cargo run -q -p compose-check -- deploy/compose/compose.yaml

edge_suite() {
  if [ "${EDGE_SUITE:-0}" != "1" ]; then
    echo "NOTE: edge behaviour suite NOT run (about 15 minutes, needs Docker). Run: EDGE_SUITE=1 bash scripts/ci.sh" >&2
    return 0
  fi
  if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
    echo "[FAIL] EDGE_SUITE=1 but Docker is unavailable" >&2
    return 1
  fi
  cargo test -p edge-tests -- --ignored --nocapture
}
stage "edge behaviour suite" edge_suite
stage "fuzz lockfile consistent" bash -c 'cd fuzz && cargo metadata --locked --format-version 1 >/dev/null'
stage "fuzz smoke" fuzz_all

STAGE="done"
if [ "$PG_SKIPPED" -eq 1 ] || [ "$FUZZ_DEGRADED" -eq 1 ]; then
  notes=""
  [ "$PG_SKIPPED" -eq 1 ] && notes="$notes PostgreSQL tests skipped (no Docker);"
  [ "$FUZZ_DEGRADED" -eq 1 ] && notes="$notes fuzz ran as proptest stand-in;"
  echo "CI OK (DEGRADED:$notes $STAGES stages passed)"
else
  echo "CI OK: $STAGES stages passed"
fi
