#!/usr/bin/env bash
# Gate: the `mutation-testing` feature of rusty-store (the SQL mutation hook used by scripts/pg-mutate.sh)
# never reaches a production build and is enabled by no manifest.
#
#   bash scripts/check-mutation-hook.sh static [ROOT]    no manifest or source enables it
#   bash scripts/check-mutation-hook.sh symbols [TARGET] a LINKED binary with the feature has the marker, release binaries do not
#   bash scripts/check-mutation-hook.sh binary EXE       one executable must not contain the marker
#
# static: fails when any Cargo.toml (root, crates/, tools/, fuzz/, deploy/) mentions
# `mutation-testing` anywhere but the definition under [features] of crates/rusty-store/Cargo.toml
# (no dependency, dev-dependency or other feature may turn it on; only the runner script passes it on
# the command line), when a Rust source outside rusty-store mentions `rusty_store::mutation` or the
# feature, or when a build/deploy file mentions it. symbols: the marker is present in a linked scratch
# binary built WITH the feature (positive control), absent from one built without it and from the release
# binaries of TARGET. binary: one executable must not contain the marker.
set -euo pipefail

mode="${1:-}"
MARKER="rusty-sql-mutation-hook-marker"

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

# symbols: three checks, all on LINKED executables (see scripts/lib-probe-binary.sh for why not rlibs).
#   1. no release executable of TARGET contains the marker;
#   2. positive control: a scratch program built WITH the feature contains it (else the gate is blind);
#   3. a scratch program built without the feature does not.
symbols_check() {
  local target="${1:-}" ctl prod fail=0 dir f
  cd "$(dirname "${BASH_SOURCE[0]}")/.."
  # shellcheck source=/dev/null
  . scripts/lib-probe-binary.sh
  dir="${CARGO_TARGET_DIR:-target}/${target:+$target/}release"
  if [ -d "$dir" ]; then
    for f in "$dir"/*; do
      [ -f "$f" ] && [ -x "$f" ] || continue
      case "$f" in *.d|*.rlib|*.so|*.a) continue ;; esac
      if binary_has_marker "$f" "$MARKER"; then
        echo "mutation hook marker in release binary $f" >&2
        fail=1
      fi
    done
  fi
  ctl="$(probe_binary "" m "$target")" || return 1
  prod="$(probe_binary "" "" "$target")" || { rm -f "$ctl"; return 1; }
  binary_has_marker "$ctl" "$MARKER" || { echo "marker is not in a binary built with the feature (gate blind)" >&2; fail=1; }
  if binary_has_marker "$prod" "$MARKER"; then
    echo "marker is present in a production-profile binary" >&2
    fail=1
  fi
  rm -f "$ctl" "$prod"
  [ "$fail" -eq 0 ] && echo "mutation hook: marker present in a linked binary built with the feature, absent from the production build and the release binaries"
  return "$fail"
}

# binary: one executable must not contain the marker (used by the self-test and by operators).
binary_check() {
  cd "$(dirname "${BASH_SOURCE[0]}")/.."
  # shellcheck source=/dev/null
  . scripts/lib-probe-binary.sh
  [ -f "${1:-}" ] || { echo "usage: $0 binary EXECUTABLE" >&2; return 2; }
  if binary_has_marker "$1" "$MARKER"; then echo "mutation hook marker in $1" >&2; return 1; fi
}

case "$mode" in
  static) static_check "${2:-}" ;;
  symbols) symbols_check "${2:-}" ;;
  binary) binary_check "${2:-}" ;;
  *) echo "usage: $0 static [ROOT] | symbols [TARGET] | binary EXECUTABLE" >&2; exit 2 ;;
esac
