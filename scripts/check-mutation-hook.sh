#!/usr/bin/env bash
# Gate: the `mutation-testing` feature of rusty-store (the SQL mutation hook used by scripts/pg-mutate.sh)
# never reaches a production build and is enabled by no manifest.
#
#   bash scripts/check-mutation-hook.sh static [ROOT]    no manifest or source enables it
#   bash scripts/check-mutation-hook.sh symbols [TARGET] a release build contains none of it
#
# static: fails when any Cargo.toml (root, crates/, tools/, fuzz/, deploy/) mentions
# `mutation-testing` anywhere but the definition under [features] of crates/rusty-store/Cargo.toml
# (no dependency, dev-dependency or other feature may turn it on; only the runner script passes it on
# the command line), when a Rust source outside rusty-store mentions `rusty_store::mutation` or the
# feature, or when a build/deploy file mentions it. symbols: marker present in a control build WITH
# the feature, absent from the production build and from the release binaries of TARGET.
set -euo pipefail

mode="${1:-}"
MARKER="rusty-mutation-testing-marker"

static_check() {
  local ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
  cd "$ROOT"
  local fail=0 out m

  # (1) other manifests: only a key of a [dev-dependencies] table may mention the token.
  while IFS= read -r m; do
    [ -n "$m" ] || continue
    [ "$m" = crates/rusty-store/Cargo.toml ] && continue
    out="$(awk '
      /^\[/ { sect = $0; next }
      /mutation-testing/ {
        if ($0 ~ /^[[:space:]]*#/) next
        print FILENAME ":" NR ": " $0 "   (in " sect ")"
      }' "$m")"
    if [ -n "$out" ]; then
      echo "mutation-testing outside rusty-store's [features] definition: $m" >&2
      printf '%s\n' "$out" >&2
      fail=1
    fi
  done < <({ [ -f Cargo.toml ] && echo Cargo.toml
             find crates tools fuzz deploy -type f -name Cargo.toml -not -path '*/target/*' 2>/dev/null | sort; })

  # (2) rusty-store's own manifest.
  if [ -f crates/rusty-store/Cargo.toml ]; then
    out="$(awk '
      /^\[/ { sect = $0; next }
      /mutation-testing/ {
        if ($0 ~ /^[[:space:]]*#/) next
        if (sect == "[features]" && $0 ~ /^[[:space:]]*mutation-testing[[:space:]]*=/) next
        print FILENAME ":" NR ": " $0 "   (in " sect ")"
      }' crates/rusty-store/Cargo.toml)"
    if [ -n "$out" ]; then
      echo "mutation-testing enabled anywhere but its [features] definition:" >&2
      printf '%s\n' "$out" >&2
      fail=1
    fi
  fi

  # (3) production sources outside rusty-store must not name the fixtures module.
  out="$(find crates tools fuzz deploy -type f -name '*.rs' -not -path '*/target/*' \
          -not -path 'crates/rusty-store/*' 2>/dev/null | sort \
        | xargs -r grep -nE 'rusty_store::mutation|mutation-testing' 2>/dev/null || true)"
  if [ -n "$out" ]; then
    echo "mutation hook used outside rusty-store:" >&2
    printf '%s\n' "$out" >&2
    fail=1
  fi

  # (4) build and deploy files.
  out="$({ for d in deploy .github .cargo; do [ -d "$d" ] && find "$d" -type f; done
           find . -maxdepth 3 \( -name 'Dockerfile*' -o -name 'Containerfile*' \) -not -path './target/*' -type f; } 2>/dev/null \
        | sort -u | xargs -r grep -n 'mutation-testing' 2>/dev/null || true)"
  if [ -n "$out" ]; then
    echo "mutation-testing in a build/deploy file:" >&2
    printf '%s\n' "$out" >&2
    fail=1
  fi
  return "$fail"
}

rlib_of() { # features [target]
  local feat="$1" target="${2:-}" args=()
  [ -z "$target" ] || args=(--target "$target")
  cargo build -p rusty-store --features "$feat" --release --lib "${args[@]}" --message-format=json 2>/dev/null \
    | { grep -o '"[^"]*/deps/librusty_store-[^"]*.rmeta"' || true; } | tail -1 | tr -d '"' | sed 's/.rmeta$/.rlib/'
}

symbols_check() {
  local target="${1:-}" prod ctl fail=0
  cd "$(dirname "${BASH_SOURCE[0]}")/.."
  prod="$(rlib_of postgres "$target")"
  ctl="$(rlib_of postgres,mutation-testing "$target")"
  [ -n "$prod" ] && [ -f "$prod" ] || { echo "no production rlib found" >&2; return 1; }
  [ -n "$ctl" ] && [ -f "$ctl" ] || { echo "no control rlib found" >&2; return 1; }
  [ "$prod" != "$ctl" ] || { echo "production and control rlib are the same file" >&2; return 1; }
  grep -aqF -- "$MARKER" "$ctl" || { echo "marker is not in the control build (gate stale)" >&2; fail=1; }
  if grep -aqF -- "$MARKER" "$prod"; then
    echo "marker is present in the production build: $prod" >&2
    fail=1
  fi
  local dir="${CARGO_TARGET_DIR:-target}/${target:+$target/}release" f
  if [ -d "$dir" ]; then
    for f in "$dir"/*; do
      [ -f "$f" ] && [ -x "$f" ] || continue
      case "$f" in *.d|*.rlib|*.so|*.a) continue ;; esac
      if grep -aqF -- "$MARKER" "$f"; then
        echo "mutation hook marker in release binary $f" >&2
        fail=1
      fi
    done
  fi
  [ "$fail" -eq 0 ] && echo "mutation-testing: marker present in the control, absent from the production build"
  return "$fail"
}

case "$mode" in
  static) static_check "${2:-}" ;;
  symbols) symbols_check "${2:-}" ;;
  *) echo "usage: $0 static [ROOT] | symbols [TARGET]" >&2; exit 2 ;;
esac
