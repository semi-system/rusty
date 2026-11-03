#!/usr/bin/env bash
# Gate: database access belongs to rusty-store. Usage: bash scripts/check-raw-db.sh [ROOT]
#
# Fails when, outside crates/rusty-store, any Rust source (crates/*/src, crates/*/tests, tools/,
# fuzz/, deploy/) mentions a raw pool or transaction: `.pool(` (whitespace tolerated),
# `::pool(` (UFCS), `.acquire(`, `begin_write`, `SqliteTx`, `SqlitePool`, `PgPool`, `sqlx`, or the
# `.store(` escape hatch of rusty-directory. Also fails when a Cargo.toml outside rusty-store
# lists `sqlx` under [dependencies]/[build-dependencies], or enables the `test-support` feature
# of rusty-directory anywhere but [dev-dependencies].
#
# Known exemption (until PG6 converts the fixtures): the rusty-directory integration tests in
# TEST_ALLOW use raw SQLite fixtures through `Directory::store()`; rusty-directory may keep
# `sqlx` as a dev-dependency. New test files are NOT exempt.
#
# What this cannot see: it is a text scan. It does not follow macros that expand to pool access,
# `include!`d files outside the scanned trees, build.rs output, re-exports under another name
# (`use sqlx as db` is caught by the word `sqlx`; a `type P = ...` alias of a pool type obtained
# from rusty-store is not), or non-Rust code. Comments are scanned too (a false positive is
# preferred over a missed use).
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

TEST_ALLOW='^crates/rusty-directory/tests/(acceptance|capabilities|directory|directory_conformance|error_leak|resolve_shape|common/mod)\.rs$'
PATTERN='\.[[:space:]]*(pool|acquire)[[:space:]]*\(|\.[[:space:]]*store[[:space:]]*\([[:space:]]*\)|::[[:space:]]*pool[[:space:]]*\(|begin_write|SqliteTx|SqlitePool|PgPool|sqlx'

fail=0
files="$(find crates tools fuzz deploy -type f -name '*.rs' \
  -not -path '*/target/*' -not -path 'crates/rusty-store/*' 2>/dev/null | sort || true)"
hits=""
while IFS= read -r f; do
  [ -n "$f" ] || continue
  if printf '%s\n' "$f" | grep -qE "$TEST_ALLOW"; then continue; fi
  h="$(grep -nE "$PATTERN" "$f" | sed "s|^|$f:|" || true)"
  [ -z "$h" ] || hits="$hits$h"$'\n'
done <<<"$files"
if [ -n "$hits" ]; then
  echo "raw pool / transaction use outside rusty-store:" >&2
  printf '%s' "$hits" >&2
  fail=1
fi

# Manifests: sqlx / test-support only where allowed.
manifests="$(find crates tools fuzz deploy -type f -name Cargo.toml -not -path '*/target/*' \
  -not -path 'crates/rusty-store/*' 2>/dev/null | sort || true)"
while IFS= read -r m; do
  [ -n "$m" ] || continue
  bad="$(awk -v dev_ok="$([ "$m" = crates/rusty-directory/Cargo.toml ] && echo 1 || echo 0)" '
    /^\[/ {
      sect = $0
      if (sect ~ /^\[(build-)?dependencies\.sqlx\]$/) print FILENAME ": sqlx in " sect
      if (sect ~ /^\[dev-dependencies\.sqlx\]$/ && dev_ok != 1) print FILENAME ": sqlx in " sect
      next
    }
    /^[[:space:]]*sqlx[[:space:]]*(=|\.)/ {
      if (sect ~ /^\[(workspace\.)?(build-)?dependencies\]$/ || sect ~ /^\[target\..*\.(build-)?dependencies\]$/) print FILENAME ": sqlx in " sect
      else if (sect ~ /^\[dev-dependencies\]$/ && dev_ok != 1) print FILENAME ": sqlx in " sect
    }
    /test-support/ && sect !~ /^\[dev-dependencies\]$/ && sect !~ /^\[features\]$/ { print FILENAME ": test-support in " sect }
  ' "$m")"
  if [ -n "$bad" ]; then
    echo "manifest violates the database boundary:" >&2
    echo "$bad" >&2
    fail=1
  fi
done <<<"$manifests"

exit "$fail"
