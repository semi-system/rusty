#!/usr/bin/env bash
# Script test: scripts/check-pg-migration-gap.sh refuses every spelling of a version 2..9 and accepts 1 and 10+.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
GATE="$PWD/scripts/check-pg-migration-gap.sh"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
expect() { # name, rc, files...
  local name="$1" want="$2"; shift 2
  rm -rf "$tmp/t"; mkdir -p "$tmp/t/crates/rusty-store/migrations_pg"
  for f in "$@"; do : > "$tmp/t/crates/rusty-store/migrations_pg/$f"; done
  (bash "$GATE" "$tmp/t" >/dev/null 2>&1); local rc=$?
  [ "$rc" = "$want" ] || { echo "pg-migration-gap: '$name': expected rc $want, got $rc" >&2; exit 1; }
}
expect "0001 then 0010 and up" 0 0001_init.sql 0010_a.sql 0011_b.sql 0012_c.sql
expect "0002_x" 1 0001_init.sql 0002_x.sql
expect "3_x without padding" 1 0001_init.sql 3_x.sql
expect "00005-y dash separator" 1 0001_init.sql 00005-y.sql
expect "09_z" 1 0001_init.sql 09_z.sql
expect "0009.sql" 1 0001_init.sql 0009.sql
expect "100_big is fine" 0 0001_init.sql 100_big.sql
expect "0020 is fine" 0 0001_init.sql 0020_x.sql
pair() { # name, rc, "sqlite files", "pg files"
  local name="$1" want="$2"
  rm -rf "$tmp/t"; mkdir -p "$tmp/t/crates/rusty-store/migrations" "$tmp/t/crates/rusty-store/migrations_pg"
  for f in $3; do : > "$tmp/t/crates/rusty-store/migrations/$f"; done
  for f in $4; do : > "$tmp/t/crates/rusty-store/migrations_pg/$f"; done
  (bash "$GATE" "$tmp/t" >/dev/null 2>&1); local rc=$?
  [ "$rc" = "$want" ] || { echo "pg-migration-gap: '$name': expected rc $want, got $rc" >&2; exit 1; }
}
pair "twins present" 0 "0001_a.sql 0002_b.sql 0010_c.sql 0016_d.sql" "0001_init.sql 0010_c.sql 0016_d.sql"
pair "sqlite 0013 without a twin" 1 "0001_a.sql 0010_c.sql 0013_u.sql" "0001_init.sql 0010_c.sql"
pair "pg-only 0016" 1 "0001_a.sql 0010_c.sql" "0001_init.sql 0010_c.sql 0016_d.sql"
pair "different names, same number is a twin" 0 "0001_a.sql 0011_x.sql" "0001_init.sql 0011_y.sql"
echo "pg-migration-gap: OK"
