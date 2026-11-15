#!/usr/bin/env bash
# Restore of a backup made by scripts/backup.sh into an EMPTY database and an EMPTY blob directory.
#
#   scripts/restore.sh --from DIR --blobs DIR [--config-dir DIR]
#
# Refuses (nothing is touched) when: the backup's checksums do not match, the blob directory exists and is not empty, or the database is not empty.
# Order: checksums, then the database, then the blobs. Afterwards run `rustyctl blobs verify` (every blob the database references must have its file) and start the services.
# The database is reached with the PG* variables; the roles of deploy/db/roles.sql must exist (the dump holds no owners and no grants: the migrator role
# re-applies them with `rustyctl db migrate`). PG_RESTORE and PSQL override the commands (for example `docker exec -i CONTAINER pg_restore -U rusty -d app`);
# the dump is read from standard input.
set -euo pipefail

from="" blobs="" config_dir=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --from) from="${2:?--from needs a directory}"; shift 2 ;;
    --blobs) blobs="${2:?--blobs needs a directory}"; shift 2 ;;
    --config-dir) config_dir="${2:?--config-dir needs a directory}"; shift 2 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "restore: unknown argument" >&2; exit 2 ;;
  esac
done
[ -n "$from" ] && [ -n "$blobs" ] || { echo "usage: restore.sh --from DIR --blobs DIR [--config-dir DIR]" >&2; exit 2; }
[ -f "$from/MANIFEST" ] && [ -f "$from/SHA256SUMS" ] && [ -f "$from/db.dump" ] || { echo "restore: $from is not a Rusty backup" >&2; exit 1; }
read -ra RESTORE <<<"${PG_RESTORE:-pg_restore}"
read -ra PSQL <<<"${PSQL:-psql}"

# 1. Checksums: a damaged or altered backup is refused before anything is written.
(cd "$from" && sha256sum --quiet -c SHA256SUMS) || { echo "restore: the checksums do not match: the backup is damaged or was changed" >&2; exit 1; }

# 2. The targets must be empty: a restore never merges into, or overwrites, live data.
if [ -e "$blobs" ] && [ -n "$(ls -A "$blobs" 2>/dev/null)" ]; then echo "restore: the blob directory $blobs is not empty" >&2; exit 1; fi
tables="$("${PSQL[@]}" -At -c "SELECT count(*) FROM pg_catalog.pg_tables WHERE schemaname NOT IN ('pg_catalog','information_schema')")"
[ "$tables" = 0 ] || { echo "restore: the database is not empty ($tables tables)" >&2; exit 1; }

# 3. The database, then the blobs.
"${RESTORE[@]}" --no-owner --no-privileges --exit-on-error < "$from/db.dump"
umask 077
mkdir -p "$blobs"
chmod 0700 "$blobs"
cp -a "$from/blobs/." "$blobs/"
chmod 0700 "$blobs"
if [ -n "$config_dir" ] && [ -d "$from/config" ]; then
  mkdir -p "$config_dir"
  cp -p -n "$from/config/." "$config_dir/" 2>/dev/null || cp -pn "$from"/config/* "$config_dir/"
fi
echo "restored. Next: rustyctl blobs verify (every referenced blob must exist), then start the services. Secrets in $from/secrets (if any) are restored by hand."
