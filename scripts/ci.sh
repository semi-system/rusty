#!/usr/bin/env bash
# Local CI gate. Run from anywhere: bash scripts/ci.sh
# Exits non-zero on the first failing stage. See SECURITY.md for tool installation.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

MUSL=x86_64-unknown-linux-musl
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
FUZZ_SECS="${FUZZ_SECS:-60}"
DEGRADED=0
export CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}"

STAGES=0
stage() { # name, command...
  local name="$1"; shift
  echo "==> $name"
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
stage "clippy"       cargo clippy --workspace --all-targets -- -D warnings
stage "test (glibc)" cargo test --workspace

postgres_tests() {
  if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
    DEGRADED=1
    echo "WARNING: Docker is unavailable; PostgreSQL smoke tests were not run." >&2
    return 0
  fi

  # A fresh run id per CI run: concurrent runs get their own container and URL file.
  RUSTY_PG_RUN_ID="ci$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
  export RUSTY_PG_RUN_ID
  local url_file="$HOME/.rusty-pg-test-url.$RUSTY_PG_RUN_ID" url rc=0
  bash scripts/pg-up.sh || return 1
  # Also clean up if CI is interrupted or exits unexpectedly during this stage.
  trap 'bash scripts/pg-down.sh >/dev/null 2>&1 || true' EXIT
  if ! IFS= read -r url < "$url_file" || [ -z "$url" ]; then
    echo "PostgreSQL URL file is missing or empty" >&2
    bash scripts/pg-down.sh
    trap - EXIT
    return 1
  fi
  for _ in 1 2 3; do
    RUSTY_TEST_PG_URL="$url" RUSTY_REQUIRE_PG=1 \
      cargo test -p rusty-store --features postgres --test pg_smoke || { rc=$?; break; }
  done
  if [ "$rc" -eq 0 ]; then
    RUSTY_TEST_PG_URL="$url" RUSTY_REQUIRE_PG=1 \
      cargo test -p rusty-store --features postgres \
        --test pg_schema_constraints --test pg_schema_parity || rc=$?
  fi
  if [ "$rc" -eq 0 ]; then
    RUSTY_TEST_PG_URL="$url" RUSTY_REQUIRE_PG=1 \
      cargo clippy -p rusty-store --features postgres --all-targets -- -D warnings || rc=$?
  fi
  bash scripts/pg-down.sh
  trap - EXIT
  return "$rc"
}
stage "postgres" postgres_tests
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
    if file -L "$f" | grep -q "statically linked"; then
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
    DEGRADED=1
    echo "WARNING: cargo-fuzz and/or the nightly toolchain are missing; running the proptest fallback only (NOT real fuzzing)." >&2
    cargo test -p rusty-mime -p rusty-directory --test fuzz_smoke
  fi
}
stage "fuzz lockfile consistent" bash -c 'cd fuzz && cargo metadata --locked --format-version 1 >/dev/null'
stage "fuzz smoke" fuzz_all

if [ "$DEGRADED" -eq 1 ]; then
  echo "CI OK (DEGRADED: fuzz ran as proptest stand-in)"
else
  echo "CI OK: $STAGES stages passed"
fi
