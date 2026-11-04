#!/usr/bin/env bash
# Gate: database access belongs to rusty-store. Usage: bash scripts/check-raw-db.sh [ROOT]
#
# Fails when, outside crates/rusty-store, any Rust source (crates/*/src, crates/*/tests, tools/,
# fuzz/, deploy/) mentions a raw pool or transaction: `.pool(` (whitespace tolerated),
# `::pool(` (UFCS), `.acquire(`, `begin_write`, `SqliteTx`, `SqlitePool`, `PgPool`, `sqlx`, or a
# `.store(` escape hatch. Also fails when a Cargo.toml outside rusty-store lists `sqlx` under any
# dependency table (dev-dependencies included).
#
# There is no exemption. Since PG6 the tests of other crates reach the database only through
# `rusty_store::fixtures` (feature `test-fixtures`, dev-dependencies only; see
# scripts/check-test-fixtures.sh), so no file outside rusty-store names a pool or a driver.
#
# What this cannot see: it is a text scan. It does not follow macros that expand to pool access,
# `include!`d files outside the scanned trees, build.rs output, re-exports under another name
# (`use sqlx as db` is caught by the word `sqlx`; a `type P = ...` alias of a pool type obtained
# from rusty-store is not), or non-Rust code. Comments are scanned too (a false positive is
# preferred over a missed use).
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

PATTERN='\.[[:space:]]*(pool|acquire)[[:space:]]*\(|\.[[:space:]]*store[[:space:]]*\([[:space:]]*\)|::[[:space:]]*pool[[:space:]]*\(|begin_write|SqliteTx|SqlitePool|PgPool|sqlx'

fail=0
files="$(find crates tools fuzz deploy -type f -name '*.rs' \
  -not -path '*/target/*' -not -path 'crates/rusty-store/*' 2>/dev/null | sort || true)"
hits=""
while IFS= read -r f; do
  [ -n "$f" ] || continue
  h="$(grep -nE "$PATTERN" "$f" | sed "s|^|$f:|" || true)"
  [ -z "$h" ] || hits="$hits$h"$'\n'
done <<<"$files"
if [ -n "$hits" ]; then
  echo "raw pool / transaction use outside rusty-store:" >&2
  printf '%s' "$hits" >&2
  fail=1
fi

# Manifests: sqlx nowhere outside rusty-store.
manifests="$(find crates tools fuzz deploy -type f -name Cargo.toml -not -path '*/target/*' \
  -not -path 'crates/rusty-store/*' 2>/dev/null | sort || true)"
while IFS= read -r m; do
  [ -n "$m" ] || continue
  bad="$(awk '
    /^\[/ {
      sect = $0
      if (sect ~ /^\[(build-|dev-)?dependencies\.sqlx\]$/) print FILENAME ": sqlx in " sect
      next
    }
    /^[[:space:]]*sqlx[[:space:]]*(=|\.)/ {
      if (sect ~ /^\[(workspace\.)?(build-|dev-)?dependencies\]$/ || sect ~ /^\[target\..*\.(build-|dev-)?dependencies\]$/) print FILENAME ": sqlx in " sect
    }
  ' "$m")"
  if [ -n "$bad" ]; then
    echo "manifest violates the database boundary:" >&2
    echo "$bad" >&2
    fail=1
  fi
done <<<"$manifests"

exit "$fail"
