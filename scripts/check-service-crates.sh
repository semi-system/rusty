#!/usr/bin/env bash
# Gate (plan section 1b rule 1): service crates talk only to the facades and repository traits.
# Usage: bash scripts/check-service-crates.sh [ROOT]
#
# Scans the Rust sources (src/ and tests/) of the service crates: rusty-svc, rusty-smtp, rusty-imap,
# rusty-ctl and the composition-root crate rusty. It fails when one of them names
#   * a concrete backend handle: SqliteStore, PgStore, SqliteRepo, PgRepo, `repo::sqlite`, `repo::pg`;
#   * an escape hatch: `.sqlite()` / `.postgres()` on a Store;
#   * a convenience constructor that takes the concrete SQLite store: `Directory::new(`,
#     `Mailbox::open(`, `JobQueue::new(`, `JobQueue::with_limits(`;
#   * a free garbage-collection function that takes `&SqliteStore` (purge_deleted, purge_deliveries,
#     purge_capabilities, collect_blobs, with their _at/_default forms): GC runs through `Gc`, built from
#     `Repos.gc`.
# Services must use `Directory::with_repo`, `Mailbox::open_with`, `JobQueue::with_repo`, `Gc::new`.
# `Store::open` is the composition root's call and is not matched. Comments are scanned too (a false
# positive is preferred to a missed use). It is a text scan: it cannot see a type alias of a concrete handle
# obtained from another crate; raw database access is the job of scripts/check-raw-db.sh.
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

PATTERN='SqliteStore|PgStore|SqliteRepo|PgRepo|repo::sqlite|repo::pg|\.[[:space:]]*(sqlite|postgres)[[:space:]]*\([[:space:]]*\)|(Directory[[:space:]]*::[[:space:]]*new|Mailbox[[:space:]]*::[[:space:]]*open|JobQueue[[:space:]]*::[[:space:]]*(new|with_limits))[[:space:]]*\(|(^|[^.A-Za-z0-9_])(purge_deleted|purge_deliveries|purge_capabilities|collect_blobs)(_at|_default)?[[:space:]]*\('

fail=0
hits=""
for c in rusty-svc rusty-smtp rusty-imap rusty-ctl rusty-runtime rusty-http rusty; do
  [ -d "crates/$c" ] || continue
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    h="$(grep -nE "$PATTERN" "$f" | sed "s|^|$f:|" || true)"
    [ -z "$h" ] || hits="$hits$h"$'\n'
  done < <(find "crates/$c/src" "crates/$c/tests" -type f -name '*.rs' 2>/dev/null | sort)
done
if [ -n "$hits" ]; then
  echo "service crates reach around the facades/traits:" >&2
  printf '%s' "$hits" >&2
  fail=1
fi
exit "$fail"
