#!/usr/bin/env bash
# Gate: migrations_pg numbering stays 0001 then 0010 and up. The Postgres history is consolidated in 0001 and its
# later twins take the SQLite number (0010 domain status, 0011 tenant rules, ...), so a file numbered 0002..0009
# would be applied AFTER an already applied 0010 on existing databases. Never fill the gap.
# Usage: bash scripts/check-pg-migration-gap.sh [ROOT]
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"
bad="$(ls crates/rusty-store/migrations_pg 2>/dev/null | grep -E '^000[2-9]_' || true)"
if [ -n "$bad" ]; then echo "migrations_pg must not contain 0002..0009:" >&2; echo "$bad" >&2; exit 1; fi
