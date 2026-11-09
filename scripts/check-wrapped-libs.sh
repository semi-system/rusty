#!/usr/bin/env bash
# Gate: third-party protocol libraries are used only inside their adapter module, so a replacement
# is one file (library-wrapper rule). Usage: bash scripts/check-wrapped-libs.sh [ROOT]
#
# For rusty-imap: `imap_codec`, `imap_types` and `chrono` may be named only in
# crates/rusty-imap/src/codec.rs. Everything else (session, writers, parsers, tests, fuzz checks)
# works on our own types from src/command.rs and is exercised through wire-level tests that do not
# import the library. Only the identifier spellings are matched, so prose may name the library.
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"
[ -d crates/rusty-imap ] || exit 0
PATTERN='imap_codec|imap_types|chrono'
hits=""
while IFS= read -r f; do
  [ -n "$f" ] || continue
  [ "$f" = "crates/rusty-imap/src/codec.rs" ] && continue
  h="$(grep -nE "$PATTERN" "$f" | sed "s|^|$f:|" || true)"
  [ -z "$h" ] || hits="$hits$h"$'\n'
done < <(find crates/rusty-imap/src crates/rusty-imap/tests -type f -name '*.rs' 2>/dev/null | sort)
if [ -n "$hits" ]; then
  echo "a wrapped library is used outside its adapter (crates/rusty-imap/src/codec.rs):" >&2
  printf '%s' "$hits" >&2
  exit 1
fi
