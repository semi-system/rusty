#!/usr/bin/env bash
# Backs up a standalone installation: the database FIRST (one consistent snapshot), then the blob volume, and with --with-keys the sealing key pair (docs/backup.md).
#
#   ./backup.sh --out /backup/2026-11-16 [--with-keys]
#
# Order is the rule: blobs are content-addressed and never change, so a blob newer than the dump is harmless (garbage collection removes it later); the other way round would leave
# messages without their file. The directory is mode 0700, must not exist or be empty, and carries SHA256SUMS of everything and a MANIFEST. Nothing is stopped: the services keep running.
#
# What this does not cover: a blob store on S3 (the bucket's own versioning or replication does that; `rustyctl blobs verify` checks after a restore that every referenced object exists),
# the edge's certificates (ACME renews them) and the configuration (setup.sh writes it again; secrets and keys that exist are kept).
# `--with-keys` adds the sealing key pair: WITHOUT IT the sealed DKIM keys, relay passwords and S3 keys in the database can never be opened again. Store that copy apart from the rest.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
ROOT="$(cd ../.. && pwd)"
OUT=""; KEYS=0
while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="${2:-}"; shift 2 ;;
    --with-keys) KEYS=1; shift ;;
    *) echo "backup: unknown argument $1 (see the header of this script)" >&2; exit 2 ;;
  esac
done
[ -n "$OUT" ] || { echo "backup: --out DIR is required" >&2; exit 2; }
[ ! -e "$OUT" ] || [ -z "$(ls -A "$OUT" 2>/dev/null)" ] || { echo "backup: $OUT exists and is not empty" >&2; exit 2; }
IMG="$(awk '$1 == "postgres:18" { print $2 }' "$ROOT/deploy/images.lock")"
[ -n "$IMG" ] || { echo "backup: postgres:18 is not pinned in deploy/images.lock" >&2; exit 1; }
STATE="$(sed -n 's/^RUSTY_STATE=//p' .env)"; [ -n "$STATE" ] || { echo "backup: run setup.sh first (no .env)" >&2; exit 1; }
PROJECT="${COMPOSE_PROJECT_NAME:-rusty}"
mkdir -p "$OUT" && chmod 0700 "$OUT"; OUT="$(cd "$OUT" && pwd)"

started_db="$(date -u +%FT%TZ)"
echo "backup: the database"
docker compose exec -T db pg_dump -U postgres -d rusty -Fc --no-owner --no-privileges > "$OUT/db.dump"
[ -s "$OUT/db.dump" ] || { echo "backup: the dump is empty" >&2; exit 1; }
started_blobs="$(date -u +%FT%TZ)"
echo "backup: the blob volume"
# The volume is read as root inside the helper, but the file is written by this shell (stdout), so it belongs to whoever runs the backup.
docker run --rm --user 0 -v "${PROJECT}_mail-data:/data:ro" --entrypoint tar "$IMG" czf - -C /data . > "$OUT/blobs.tar.gz"
if [ "$KEYS" = 1 ]; then
  echo "backup: the sealing key pair (keep this file apart from the rest)"
  docker run --rm --user 0 -v "$STATE/keys:/keys:ro" --entrypoint tar "$IMG" czf - -C /keys . > "$OUT/keys.tar.gz"
fi
{
  echo "rusty standalone backup"
  echo "database dumped first: $started_db"
  echo "blob volume copied after: $started_blobs"
  echo "images: $(docker compose config | sed -n 's/^ *image: //p' | grep -E '/rusty:|/rusty-' | head -1)"
  echo "keys included: $KEYS"
} > "$OUT/MANIFEST"
( cd "$OUT" && sha256sum -- * > SHA256SUMS.tmp && mv SHA256SUMS.tmp SHA256SUMS )
chmod 0600 "$OUT"/*
echo "backup: done in $OUT"
