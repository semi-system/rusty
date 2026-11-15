#!/usr/bin/env bash
# Backup of a Rusty installation: the database, then the blob store, then the configuration.
#
#   scripts/backup.sh --out DIR --blobs DIR [--config-dir DIR] [--include-secrets KEYDIR...]
#
# ORDER MATTERS. The database is dumped FIRST (one consistent snapshot, pg_dump), the blob store is copied AFTER. Blobs are content-addressed and
# immutable, so a blob newer than the dump is harmless; the reverse (a dump newer than the blob copy) would leave messages with no file.
# The backup directory is made 0700, must not exist or must be empty, and carries SHA256SUMS of everything and a manifest with the order of the steps.
#
# The database is reached with the usual PG* variables (PGHOST, PGPORT, PGUSER, PGPASSWORD or ~/.pgpass, PGDATABASE). Use a role that can read every
# table (the owner or a superuser; `rusty_ro` cannot read the sealed secrets, by design). PG_DUMP overrides the command (for example
# `docker exec -i CONTAINER pg_dump -U rusty`): the dump is written to standard output, so it works through a container.
#
# Secrets are NOT copied unless named: --include-secrets KEYDIR copies a directory of private keys (an unsealing key, DKIM key files) with mode 0600 files.
# TLS keys at the edge are the edge's own and are not part of this backup.
set -euo pipefail

out="" blobs="" config_dir="" secrets=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --out) out="${2:?--out needs a directory}"; shift 2 ;;
    --blobs) blobs="${2:?--blobs needs a directory}"; shift 2 ;;
    --config-dir) config_dir="${2:?--config-dir needs a directory}"; shift 2 ;;
    --include-secrets) secrets+=("${2:?--include-secrets needs a directory}"); shift 2 ;;
    -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
    *) echo "backup: unknown argument" >&2; exit 2 ;;
  esac
done
[ -n "$out" ] && [ -n "$blobs" ] || { echo "usage: backup.sh --out DIR --blobs DIR [--config-dir DIR] [--include-secrets DIR]..." >&2; exit 2; }
[ -d "$blobs" ] || { echo "backup: the blob directory does not exist" >&2; exit 1; }
[ -f "$blobs/.rusty-blobs" ] || { echo "backup: $blobs is not a Rusty blob root (marker file missing)" >&2; exit 1; }
if [ -e "$out" ] && [ -n "$(ls -A "$out" 2>/dev/null)" ]; then echo "backup: $out exists and is not empty" >&2; exit 1; fi

read -ra DUMP <<<"${PG_DUMP:-pg_dump}"
umask 077
mkdir -p "$out"
chmod 0700 "$out"

stamp() { date -u +%Y-%m-%dT%H:%M:%SZ; }
started="$(stamp)"

# 1. The database, first.
db_started="$(stamp)"
"${DUMP[@]}" --format=custom --no-owner --no-privileges > "$out/db.dump"
[ -s "$out/db.dump" ] || { echo "backup: the database dump is empty" >&2; exit 1; }
db_done="$(stamp)"

# 2. The blobs, after.
blobs_started="$(stamp)"
mkdir -p "$out/blobs"
cp -a "$blobs/." "$out/blobs/"
chmod 0700 "$out/blobs"
blobs_done="$(stamp)"

# 3. Configuration (never a TLS key) and the named secret directories.
if [ -n "$config_dir" ]; then
  mkdir -p "$out/config"
  find "$config_dir" -maxdepth 1 -type f \( -name '*.toml' -o -name '*.example' \) -exec cp -p {} "$out/config/" \;
fi
i=0
for s in "${secrets[@]+"${secrets[@]}"}"; do
  i=$((i + 1))
  mkdir -p "$out/secrets/$i"
  cp -a "$s/." "$out/secrets/$i/"
  chmod -R go-rwx "$out/secrets"
done

# 4. The manifest and the checksums of everything in the backup.
cat > "$out/MANIFEST" <<M
rusty-backup 1
started $started
database-dump-started $db_started
database-dump-done $db_done
blobs-copy-started $blobs_started
blobs-copy-done $blobs_done
order database-first-then-blobs
M
(cd "$out" && find . -type f ! -name SHA256SUMS | LC_ALL=C sort | while IFS= read -r f; do sha256sum "$f"; done > SHA256SUMS)
echo "backup written to $out ($(wc -l < "$out/SHA256SUMS") files); verify a restore with: rustyctl blobs verify"
