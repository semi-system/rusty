#!/usr/bin/env bash
# Gate: the migration numbering stays 0001 then 0010 and up. The history was consolidated in 0001 and the later
# migrations kept their numbers (0010 domain status, 0011 tenant rules, ...), so a file numbered 0002..0009 would be
# applied AFTER an already applied 0010 on existing databases. Never fill the gap.
# Usage: bash scripts/check-pg-migration-gap.sh [ROOT]
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"
bad="$(cd crates/rusty-store/migrations 2>/dev/null && printf "%s
" * | grep -E "^0*[2-9]([^0-9]|$)" || true)"
if [ -n "$bad" ]; then echo "migrations must not contain 0002..0009:" >&2; echo "$bad" >&2; exit 1; fi
