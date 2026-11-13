#!/usr/bin/env bash
# Gate: migrations_pg numbering stays 0001 then 0010 and up. The Postgres history is consolidated in 0001 and its
# later twins take the SQLite number (0010 domain status, 0011 tenant rules, ...), so a file numbered 0002..0009
# would be applied AFTER an already applied 0010 on existing databases. Never fill the gap.
# Usage: bash scripts/check-pg-migration-gap.sh [ROOT]
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"
bad="$(cd crates/rusty-store/migrations_pg 2>/dev/null && printf "%s
" * | grep -E "^0*[2-9]([^0-9]|$)" || true)"
if [ -n "$bad" ]; then echo "migrations_pg must not contain 0002..0009:" >&2; echo "$bad" >&2; exit 1; fi

# Twin rule (U11): every SQLite migration numbered 0010 or higher has a Postgres migration with the SAME
# number (the pair is documented in docs/postgres-production.md, "Migration numbering"), and every
# Postgres migration from 0010 up has a SQLite twin. Skipped when there is no SQLite directory (self-test trees).
if [ -d crates/rusty-store/migrations ]; then
  num() { sed -E 's/^0*([0-9]+).*/\1/'; }
  lite="$(ls crates/rusty-store/migrations | num | awk '$1 >= 10' | sort -n)"
  pg="$(ls crates/rusty-store/migrations_pg | num | awk '$1 >= 10' | sort -n)"
  missing_pg="$(comm -23 <(echo "$lite") <(echo "$pg") | tr '\n' ' ')"
  missing_lite="$(comm -13 <(echo "$lite") <(echo "$pg") | tr '\n' ' ')"
  [ -z "${missing_pg// /}" ] || { echo "SQLite migrations without a Postgres twin of the same number: $missing_pg" >&2; exit 1; }
  [ -z "${missing_lite// /}" ] || { echo "Postgres migrations without a SQLite twin of the same number: $missing_lite" >&2; exit 1; }
fi
