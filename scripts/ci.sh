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
KV_RUN_ID=""

# One EXIT trap for the whole script, never cleared: it tears down this run's Postgres container
# (if one is up) and reports the stage that was active when the script exited non-zero, also for
# failures that happen outside stage() (set -e, missing tools, signals turned into exits).
on_exit() {
  local rc=$?
  trap - EXIT
  if [ -n "$PG_RUN_ID" ]; then
    RUSTY_PG_RUN_ID="$PG_RUN_ID" bash scripts/pg-down.sh >/dev/null 2>&1 || true
  fi
  if [ -n "$KV_RUN_ID" ]; then
    RUSTY_KV_RUN_ID="$KV_RUN_ID" bash scripts/kv-down.sh >/dev/null 2>&1 || true
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
  local t0=$SECONDS
  if "$@"; then
    STAGES=$((STAGES + 1))
    echo "[ OK ] $name ($((SECONDS - t0))s)"
  else
    echo "[FAIL] $name ($((SECONDS - t0))s)" >&2
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
stage "remote build helpers self-test" bash scripts/tests/remote-lib.sh
# CI speed helpers (docs/ci.md): change classification, cache pruning, pinned tool install.
stage "ci-changes self-test" bash scripts/tests/ci-changes.sh
stage "ci-cache-prune self-test" bash scripts/tests/ci-cache-prune.sh
stage "ci-tool self-test" bash scripts/tests/ci-tool.sh
# Pre-push check (run bash scripts/prepush-check.sh yourself before every git push).
stage "prepush-check self-test" bash scripts/tests/prepush-check.sh

# Database access belongs to rusty-store (scripts/check-raw-db.sh documents what the text scan
# covers and what it cannot see). The gate has its own self-test.
stage "raw-db gate self-test" bash scripts/tests/raw-db-gate.sh
stage "no raw pool outside store" bash scripts/check-raw-db.sh
# Raw SQL fixtures for other crates (rusty_store::fixtures) exist only in dev-dependency builds.
stage "test-fixtures gate self-test" bash scripts/tests/test-fixtures-gate.sh
stage "no test fixtures outside dev-dependencies" bash scripts/check-test-fixtures.sh static
# The SQL mutation hook (scripts/pg-mutate.sh) is enabled by no manifest and absent from release builds.
stage "mutation-hook gate self-test" bash scripts/tests/mutation-hook-gate.sh
stage "no mutation hook anywhere" bash scripts/check-mutation-hook.sh static
# Service crates reach storage only through the facades and repository traits (plan section 1b rule 1).
stage "pg-migration-gap gate self-test" bash scripts/tests/pg-migration-gap.sh
stage "no filled-in postgres migration gap" bash scripts/check-pg-migration-gap.sh
stage "wrapped-libs gate self-test" bash scripts/tests/wrapped-libs-gate.sh
stage "wrapped libraries stay behind their adapter" bash scripts/check-wrapped-libs.sh
stage "service-crates gate self-test" bash scripts/tests/service-crates-gate.sh
stage "service crates use only facades" bash scripts/check-service-crates.sh
# The `test-knobs` fault-injection feature of rusty-store is enabled only by its own tests.
stage "test-knobs gate self-test" bash scripts/tests/test-knobs-gate.sh
stage "no test knobs outside tests" bash scripts/check-test-knobs.sh static
# Supply chain: Dockerfile bases and workflow actions are pinned by digest/SHA; workflows and shell
# scripts are linted in pinned containers (skips with an explicit note when Docker is absent).
stage "docker-pins gate self-test" bash scripts/tests/docker-pins-gate.sh
stage "docker and workflow pins" bash scripts/check-docker-pins.sh
stage "workflows-lint self-test" bash scripts/tests/workflows-lint.sh
stage "actionlint and shellcheck" bash scripts/ci-workflows-lint.sh
stage "clippy"       cargo clippy --workspace --all-targets -- -D warnings
# Postgres is the only backend, so every test stage needs a server. One throw-away container
# (scripts/pg-up.sh) serves the whole run; a fresh run id gives concurrent runs their own container and
# URL file, and on_exit removes exactly this run's container, even if a stage dies half-way.
pg_start() {
  if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
    echo "Docker is required: the tests run on a throw-away PostgreSQL (scripts/pg-up.sh)." >&2
    return 1
  fi
  RUSTY_PG_RUN_ID="ci$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
  export RUSTY_PG_RUN_ID
  PG_RUN_ID="$RUSTY_PG_RUN_ID"
  local url_file="$HOME/.rusty-pg-test-url.$RUSTY_PG_RUN_ID" url
  bash scripts/pg-up.sh || return 1
  if ! IFS= read -r url < "$url_file" || [ -z "$url" ]; then
    echo "PostgreSQL URL file is missing or empty" >&2
    return 1
  fi
  export RUSTY_TEST_PG_URL="$url"
}
stage "postgres test server" pg_start

# The throw-away Valkey (scripts/kv-up.sh) for the rusty-valkey tests and every consumer of the shared key-value store;
# a fresh run id, removed by on_exit like the Postgres container.
kv_start() {
  KV_RUN_ID="ci$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
  export RUSTY_KV_RUN_ID="$KV_RUN_ID"
  local url_file="$HOME/.rusty-kv-test-url.$KV_RUN_ID" url
  bash scripts/kv-up.sh || return 1
  if ! IFS= read -r url < "$url_file" || [ -z "$url" ]; then
    echo "Valkey URL file is missing or empty" >&2
    return 1
  fi
  export RUSTY_TEST_VALKEY_URL="$url"
  export RUSTY_REQUIRE_VALKEY=1
}
stage "valkey test server" kv_start

stage "test (glibc)" cargo test --workspace
# The dev quickstart works from a clean checkout: build, build the schema, start on spare ports, banner, SIGTERM, exit 0.
# The dev profile names its database in RUSTY_DEV_PG_URL / RUSTY_DEV_PG_PASSWORD: here, the test server above.
quickstart_smoke() {
  (
    u="${RUSTY_TEST_PG_URL#postgres://}"
    cred="${u%%@*}"
    rest="${u#*@}"
    export RUSTY_DEV_PG_URL="postgres://${cred%%:*}@${rest%%\?*}"
    export RUSTY_DEV_PG_PASSWORD="${cred#*:}"
    bash deploy/dev/quickstart.sh --smoke
  )
}
stage "dev quickstart smoke" quickstart_smoke

# The plain suites already ran in "test (glibc)" against the server above. What is left needs a special
# setup: the smoke test, the suites again as the unprivileged runtime role rusty_app (a database initialised
# with deploy/db/roles.sql and migrated by rusty_migrator, what production runs), the mutation hook's table
# and probes (the full mutation run is scripts/pg-mutate.sh, not a CI stage), and clippy with the mutation feature.
postgres_tests() {
  local rc=0
  cargo test -p rusty-store --test pg_smoke || rc=$?
  if [ "$rc" -eq 0 ]; then
    RUSTY_TEST_PG_ROLES=1 \
      cargo test -p rusty-store --test mailbox_conformance --test jobs_conformance \
        --test gc_conformance --test imap_ops_conformance --test input_validation_conformance --test race_conformance \
        --test mailbox --test jobs --test gc --test deliveries --test batch --test mailbox_paging || rc=$?
  fi
  if [ "$rc" -eq 0 ]; then
    RUSTY_TEST_PG_ROLES=1 cargo test -p rusty-directory -p rusty-imap -p rusty-smtp -p rusty-ctl || rc=$?
  fi
  if [ "$rc" -eq 0 ]; then
    cargo test -p rusty-store --features mutation-testing --test pg_dialect_mutations || rc=$?
  fi
  if [ "$rc" -eq 0 ]; then
    cargo clippy -p rusty-store -p rusty-directory -p rusty-imap -p rusty-smtp -p rusty-ctl --all-targets -- -D warnings || rc=$?
  fi
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
# A release build of rusty-store contains none of the knob code (positive control included).
stage "no test knobs in release" bash scripts/check-test-knobs.sh symbols "$MUSL"
stage "no test fixtures in release" bash scripts/check-test-fixtures.sh symbols "$MUSL"
stage "no mutation hook in release" bash scripts/check-mutation-hook.sh symbols "$MUSL"

# The end-to-end scenario against a throw-away Postgres with TLS and roles (needs the musl release binary built above
# and Docker).
acceptance() {
  local bin="$TARGET_DIR/$MUSL/release/rusty"
  RUSTY_BIN="$bin" bash tools/acceptance/local-run.sh
}
stage "acceptance" acceptance

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
    local t nightly_tc
    # Use the installed nightly by its exact name (e.g. nightly-2026-11-01-...): plain `+nightly` makes rustup
    # download the latest channel when only a dated nightly is installed.
    nightly_tc="$(rustup toolchain list | awk '/^nightly/{print $1; exit}')"
    for t in parse_envelope parse_address fuzz_config fuzz_pg_url fuzz_pem fuzz_ctl_args fuzz_smtp_command fuzz_smtp_data fuzz_smtp_path fuzz_smtp_headers fuzz_smtp_auth fuzz_smtp_response fuzz_dsn_headers fuzz_queue_payload fuzz_dkim_key fuzz_imap_command fuzz_imap_structure fuzz_imap_search_eval fuzz_imap_seqset fuzz_imap_utf7; do
      mkdir -p "fuzz/corpus/$t"
      # Working corpus first (new finds are written there), committed seeds second.
      (cd fuzz && cargo "+$nightly_tc" fuzz run "$t" "corpus/$t" "seeds/$t" -- "-max_total_time=$FUZZ_SECS" -rss_limit_mb=2048) || return 1
    done
  else
    FUZZ_DEGRADED=1
    echo "WARNING: cargo-fuzz and/or the nightly toolchain are missing; running the proptest fallback only (NOT real fuzzing)." >&2
    cargo test -p rusty-mime -p rusty-directory -p rusty-core -p rusty-svc -p rusty-ctl -p rusty-smtp -p rusty-imap --test fuzz_smoke
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
