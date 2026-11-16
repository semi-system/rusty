#!/usr/bin/env bash
# Restores a backup made by backup.sh into a FRESH installation (docs/backup.md): the database and the blob volume are empty, the roles are made, the dump is restored as the schema owner,
# the blobs are unpacked, and the next start applies the privileges again.
#
#   ./setup.sh ...same arguments as the installation...      # a fresh state directory, or the old one for the same secrets
#   docker compose down -v                                  # a restore never merges into live data: the volumes must be new
#   ./restore.sh --from /backup/2026-11-16 [--with-keys]
#   docker compose up -d && docker compose run --rm --name rusty-ctl-1 ctl blobs verify
#
# It verifies SHA256SUMS before it touches anything and refuses a database that already has tables or a blob volume that is not empty.
# `--with-keys` unpacks keys.tar.gz into the state directory (only when no sealing key is there yet).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
ROOT="$(cd ../.. && pwd)"
FROM=""; KEYS=0
while [ $# -gt 0 ]; do
  case "$1" in
    --from) FROM="${2:-}"; shift 2 ;;
    --with-keys) KEYS=1; shift ;;
    *) echo "restore: unknown argument $1 (see the header of this script)" >&2; exit 2 ;;
  esac
done
[ -d "$FROM" ] || { echo "restore: --from DIR (a backup made by backup.sh) is required" >&2; exit 2; }
FROM="$(cd "$FROM" && pwd)"
IMG="$(awk '$1 == "postgres:18" { print $2 }' "$ROOT/deploy/images.lock")"
STATE="$(sed -n 's/^RUSTY_STATE=//p' .env)"; [ -n "$STATE" ] || { echo "restore: run setup.sh first (no .env)" >&2; exit 1; }
PROJECT="${COMPOSE_PROJECT_NAME:-rusty}"

echo "restore: verifying the backup"
( cd "$FROM" && sha256sum -c SHA256SUMS >/dev/null ) || { echo "restore: the backup is damaged (SHA256SUMS does not match); nothing was changed" >&2; exit 1; }
[ -s "$FROM/db.dump" ] && [ -s "$FROM/blobs.tar.gz" ] || { echo "restore: db.dump or blobs.tar.gz is missing" >&2; exit 1; }
[ "$KEYS" = 0 ] || [ -s "$FROM/keys.tar.gz" ] || { echo "restore: --with-keys but the backup has no keys.tar.gz" >&2; exit 1; }

echo "restore: the database and the blob volume must be empty"
docker compose up -d db >/dev/null
for _ in $(seq 1 60); do [ "$(docker compose ps db --format '{{.Health}}')" = healthy ] && break; sleep 2; done
# `compose run` names the container at random and the database allows `rusty-db-init-1` only (docs/docker.md), so the name is given.
docker rm -f rusty-db-init-1 >/dev/null 2>&1 || true
docker compose run --rm --name rusty-db-init-1 db-init >/dev/null
tables="$(docker compose exec -T db psql -U postgres -d rusty -Atc "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'public'")"
[ "$tables" = 0 ] || { echo "restore: the database already has $tables tables; start from new volumes (docker compose down -v). Nothing was changed." >&2; exit 1; }
files="$(docker run --rm --user 0 -v "${PROJECT}_mail-data:/data:ro" --entrypoint sh "$IMG" -c 'ls -A /data | wc -l')"
[ "$files" = 0 ] || { echo "restore: the blob volume is not empty. Nothing was changed." >&2; exit 1; }

echo "restore: the database (as the schema owner, so the next start can apply the privileges)"
docker compose exec -T db pg_restore -U postgres --role=rusty_migrator --no-owner --no-privileges -d rusty --exit-on-error < "$FROM/db.dump"
echo "restore: the blobs"
docker run --rm --user 0 -v "${PROJECT}_mail-data:/data" -v "$FROM:/in:ro" --entrypoint tar "$IMG" xzf /in/blobs.tar.gz -C /data
if [ "$KEYS" = 1 ]; then
  if docker run --rm --user 0 -v "$STATE/keys:/keys:ro" --entrypoint sh "$IMG" -c 'test -e /keys/unseal.key'; then
    echo "restore: a sealing key is already in $STATE/keys; not overwritten" >&2
  else
    docker run --rm --user 0 -v "$STATE/keys:/keys" -v "$FROM:/in:ro" --entrypoint tar "$IMG" xzf /in/keys.tar.gz -C /keys
    echo "restore: the sealing key pair is back in $STATE/keys"
  fi
fi
echo "restore: done. Now: docker compose up -d && docker compose run --rm --name rusty-ctl-1 ctl blobs verify"
