#!/usr/bin/env bash
# Local CI gate. Run from anywhere: bash scripts/ci.sh
# Exits non-zero on the first failing stage. See SECURITY.md for tool installation.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

MUSL=x86_64-unknown-linux-musl
FUZZ_SECS="${FUZZ_SECS:-60}"
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
stage "cargo-deny"   cargo deny check
stage "cargo-audit"  cargo audit
stage "test (musl)"  cargo test --workspace --target "$MUSL"
stage "build (musl release)" cargo build --release --workspace --target "$MUSL"

check_static() {
  local found=0 f
  shopt -s nullglob
  for f in target/"$MUSL"/release/*; do
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

DEGRADED=0
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
# Not via stage(): a subshell would lose DEGRADED.
echo "==> fuzz smoke"
if fuzz_all; then
  STAGES=$((STAGES + 1))
  echo "[ OK ] fuzz smoke"
else
  echo "[FAIL] fuzz smoke" >&2
  exit 1
fi

if [ "$DEGRADED" -eq 1 ]; then
  echo "CI OK (DEGRADED: fuzz ran as proptest stand-in)"
else
  echo "CI OK: $STAGES stages passed"
fi
