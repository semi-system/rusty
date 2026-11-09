#!/usr/bin/env bash
# Gate (plan section 1b): a wrapped third-party protocol library is named only inside its adapter module.
# Usage: bash scripts/check-wrapped-libs.sh [ROOT]
# Config: scripts/wrapped-libs.txt, one "<crate> <adapter dir>" per line. For each library the gate fails when its
# name (Rust form with underscores or Cargo form with hyphens) appears in any .rs file under crates/, tools/ or fuzz/
# outside the adapter directory (comments included: a false positive is preferred to a missed use), or when a
# Cargo.toml names it outside the manifest of the crate that owns the adapter and the workspace root manifest.
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"
cfg=scripts/wrapped-libs.txt
[ -f "$cfg" ] || { echo "check-wrapped-libs: $cfg is missing" >&2; exit 2; }
fail=0
while read -r lib adapter _; do
  case "${lib:-}" in ''|'#'*) continue ;; esac
  [ -n "${adapter:-}" ] || { echo "check-wrapped-libs: line for '$lib' has no adapter dir" >&2; exit 2; }
  us="${lib//-/_}"
  owner="${adapter%%/src/*}"
  hits="$(grep -rnE "(^|[^A-Za-z0-9_])(${us}|${lib})([^A-Za-z0-9_]|$)" crates tools fuzz --include='*.rs' 2>/dev/null | grep -v "^${adapter}/" | grep -v '/target/' || true)"
  if [ -n "$hits" ]; then echo "wrapped library '$lib' used outside $adapter:" >&2; printf '%s\n' "$hits" >&2; fail=1; fi
  mh="$(grep -rnE "^[[:space:]]*${lib}[[:space:]]*(=|\.)" --include=Cargo.toml crates tools fuzz 2>/dev/null | grep -v "^${owner}/Cargo.toml:" | grep -v '/target/' || true)"
  if [ -n "$mh" ]; then echo "wrapped library '$lib' declared outside $owner/Cargo.toml:" >&2; printf '%s\n' "$mh" >&2; fail=1; fi
done < "$cfg"
exit "$fail"
