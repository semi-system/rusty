#!/usr/bin/env bash
# Gate: the `test-knobs` fault-injection feature of rusty-store never reaches a release build.
#
#   bash scripts/check-test-knobs.sh static [ROOT]    nothing outside rusty-store's own tests enables it
#   bash scripts/check-test-knobs.sh symbols [TARGET] a release build contains none of the knob code
#
# static: fails when
#   * any Cargo.toml other than crates/rusty-store/Cargo.toml (root, crates/, tools/, fuzz/,
#     deploy/) mentions `test-knobs`;
#   * crates/rusty-store/Cargo.toml mentions it anywhere but (a) its own definition
#     `test-knobs = [...]` under [features] and (b) the self dev-dependency
#     `rusty-store = { path = ".", features = ["test-knobs"] }` under [dev-dependencies]
#     (so `default = ["test-knobs"]`, `postgres = [.., "test-knobs"]` or a [dependencies] entry fail);
#   * a Rust source outside crates/rusty-store mentions `test-knobs` or one of the knob setters
#     (with_knobs_for_test, inject_deadlocks_for_test, with_work_done_signal_for_test);
#   * a build/deploy file (deploy/, .github/, .cargo/, Dockerfile*, Containerfile*) mentions it.
# What it cannot see: a feature enabled by an environment variable or command line outside the
# repository (CARGO_FEATURE flags, `cargo build --features rusty-store/test-knobs` typed by hand);
# the symbols mode is the backstop for those.
#
# symbols: builds rusty-store (--release) and the same crate WITH the
# feature as a positive control, and requires every marker (SQL text and identifiers that exist
# only in the knob code) to be present in the control and absent in the production build. A marker
# missing from the control means the gate went stale and fails too. TARGET (optional) is a rustc
# target triple; the release binaries of that target are scanned for the identifiers as well.
set -euo pipefail

mode="${1:-}"

static_check() {
  local ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
  cd "$ROOT"
  local fail=0 out m

  # (1) other manifests (root included): the token must not appear at all.
  while IFS= read -r m; do
    [ -n "$m" ] || continue
    [ "$m" = crates/rusty-store/Cargo.toml ] && continue
    out="$(grep -n 'test-knobs' "$m" || true)"
    if [ -n "$out" ]; then
      echo "test-knobs in a manifest other than rusty-store's: $m" >&2
      printf '%s\n' "$out" >&2
      fail=1
    fi
  done < <({ [ -f Cargo.toml ] && echo Cargo.toml
             find crates tools fuzz deploy -type f -name Cargo.toml -not -path '*/target/*' 2>/dev/null | sort; })

  # (2) rusty-store's own manifest: definition under [features], self dev-dependency only.
  if [ -f crates/rusty-store/Cargo.toml ]; then
    out="$(awk '
      /^\[/ { sect = $0; next }
      /test-knobs/ {
        if (sect == "[features]" && $0 ~ /^[[:space:]]*test-knobs[[:space:]]*=/ && $0 !~ /=.*test-knobs.*test-knobs/) next
        if (sect == "[dev-dependencies]" && $0 ~ /^[[:space:]]*rusty-store[[:space:]]*=/ && $0 ~ /path[[:space:]]*=[[:space:]]*"\."/) next
        if ($0 ~ /^[[:space:]]*#/) next
        print FILENAME ":" NR ": " $0 "   (in " sect ")"
      }' crates/rusty-store/Cargo.toml)"
    if [ -n "$out" ]; then
      echo "test-knobs enabled outside rusty-store's own [dev-dependencies]:" >&2
      printf '%s\n' "$out" >&2
      fail=1
    fi
  fi

  # (3) Rust sources outside rusty-store.
  out="$(find crates tools fuzz deploy -type f -name '*.rs' -not -path '*/target/*' \
          -not -path 'crates/rusty-store/*' 2>/dev/null | sort \
        | xargs -r grep -nE 'test-knobs|with_knobs_for_test|inject_deadlocks_for_test|with_work_done_signal_for_test' 2>/dev/null || true)"
  if [ -n "$out" ]; then
    echo "test knob API used outside rusty-store:" >&2
    printf '%s\n' "$out" >&2
    fail=1
  fi

  # (4) build and deploy files.
  out="$({ for d in deploy .github .cargo; do [ -d "$d" ] && find "$d" -type f; done
           find . -maxdepth 3 \( -name 'Dockerfile*' -o -name 'Containerfile*' \) -not -path './target/*' -type f; } 2>/dev/null \
        | sort -u | xargs -r grep -n 'test-knobs' 2>/dev/null || true)"
  if [ -n "$out" ]; then
    echo "test-knobs in a build/deploy file:" >&2
    printf '%s\n' "$out" >&2
    fail=1
  fi
  return "$fail"
}

# Markers that exist only in knob code: SQL text and identifiers.
MARKERS=(pg_sleep injected with_knobs_for_test inject_deadlocks_for_test with_work_done_signal_for_test)

rlib_of() { # features (may be empty) [target]
  local feat="$1" target="${2:-}" args=()
  [ -z "$target" ] || args=(--target "$target")
  [ -z "$feat" ] || args+=(--features "$feat")
  cargo build -p rusty-store --release --lib "${args[@]}" --message-format=json 2>/dev/null \
    | { grep -o '"[^"]*/deps/librusty_store-[^"]*.rmeta"' || true; } | tail -1 | tr -d '"' | sed 's/.rmeta$/.rlib/'
}

symbols_check() {
  local target="${1:-}" prod ctl fail=0 m
  cd "$(dirname "${BASH_SOURCE[0]}")/.."
  prod="$(rlib_of "" "$target")"
  ctl="$(rlib_of test-knobs "$target")"
  [ -n "$prod" ] && [ -f "$prod" ] || { echo "no production rlib found" >&2; return 1; }
  [ -n "$ctl" ] && [ -f "$ctl" ] || { echo "no control rlib found" >&2; return 1; }
  [ "$prod" != "$ctl" ] || { echo "production and control rlib are the same file" >&2; return 1; }
  for m in "${MARKERS[@]}"; do
    if ! grep -aqF -- "$m" "$ctl"; then
      echo "marker '$m' is not in the control build (gate stale)" >&2
      fail=1
    fi
    if grep -aqF -- "$m" "$prod"; then
      echo "marker '$m' is present in the production build: $prod" >&2
      fail=1
    fi
  done
  # Release binaries of the target (rusty-store with postgres may be linked into one one day).
  local dir="${CARGO_TARGET_DIR:-target}/${target:+$target/}release" f
  if [ -d "$dir" ]; then
    for f in "$dir"/*; do
      [ -f "$f" ] && [ -x "$f" ] || continue
      case "$f" in *.d|*.rlib|*.so|*.a) continue ;; esac
      if grep -aqE 'with_knobs_for_test|inject_deadlocks_for_test|skip_mailbox_lock|signal_work_done' "$f"; then
        echo "knob identifiers in release binary $f" >&2
        fail=1
      fi
    done
  fi
  [ "$fail" -eq 0 ] && echo "test-knobs: ${#MARKERS[@]} markers present in the control, absent from the production build"
  return "$fail"
}

case "$mode" in
  static) static_check "${2:-}" ;;
  symbols) symbols_check "${2:-}" ;;
  *) echo "usage: $0 static [ROOT] | symbols [TARGET]" >&2; exit 2 ;;
esac
