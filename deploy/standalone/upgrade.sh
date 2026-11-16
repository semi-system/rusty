#!/usr/bin/env bash
# Upgrades a standalone installation to another image tag the way docs/operations.md describes: a backup first, setup.sh again with the arguments of this installation (it pulls the images and
# keeps the secrets and keys that exist), `docker compose up -d` with the number of replicas that run now, then a health check and the schema check.
#
#   ./upgrade.sh --image-tag 1.2.3 [--backup-dir DIR | --no-backup] [--with-keys] [--timeout SECONDS]
#
# A backup is the only way back after a migration (there is no down migration), so it is the default: $STATE/backups/before-<tag>-<time>. `--with-keys` adds the sealing key pair to it (a copy next to the
# original protects nothing: give a directory somewhere else). The arguments of the installation are in $STATE/setup.args, written by setup.sh; an installation made by an older setup.sh has none yet:
# run setup.sh once with its arguments, then this script. For a development installation (`--build`) there is nothing to pull: use setup.sh.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
TAG=""; BACKUP_DIR=""; BACKUP=1; KEYS=0; TIMEOUT=300
while [ $# -gt 0 ]; do
  case "$1" in
    --image-tag) TAG="${2:-}"; shift 2 ;;
    --backup-dir) BACKUP_DIR="${2:-}"; shift 2 ;;
    --no-backup) BACKUP=0; shift ;;
    --with-keys) KEYS=1; shift ;;
    --timeout) TIMEOUT="${2:-}"; shift 2 ;;
    *) echo "upgrade: unknown argument $1 (see the header of this script)" >&2; exit 2 ;;
  esac
done
[[ "$TAG" =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$ ]] || { echo "upgrade: --image-tag must be a registry tag (1.2.3, sha-abcdef0, main)" >&2; exit 2; }
[[ "$TIMEOUT" =~ ^[0-9]+$ ]] || { echo "upgrade: --timeout is a number of seconds" >&2; exit 2; }
[ "$BACKUP" = 0 ] || [ -z "$BACKUP_DIR" ] || [ ! -e "$BACKUP_DIR" ] || [ -z "$(ls -A "$BACKUP_DIR" 2>/dev/null)" ] || { echo "upgrade: $BACKUP_DIR exists and is not empty" >&2; exit 2; }
STATE="$(sed -n 's/^RUSTY_STATE=//p' .env)"; [ -n "$STATE" ] || { echo "upgrade: run setup.sh first (no .env)" >&2; exit 1; }
[ -s "$STATE/setup.args" ] || { echo "upgrade: $STATE/setup.args is missing: this installation was made by an older setup.sh. Run ./setup.sh once with its arguments, then this script." >&2; exit 1; }
mapfile -t SAVED < "$STATE/setup.args"
# The arguments of the installation without its old image tag; the new tag goes last.
ARGS=(); i=0
while [ $i -lt ${#SAVED[@]} ]; do
  case "${SAVED[$i]}" in
    --image-tag) i=$((i + 2)) ;;
    --build) echo "upgrade: this installation builds its images (--build): there is nothing to pull; use setup.sh" >&2; exit 1 ;;
    *) ARGS+=("${SAVED[$i]}"); i=$((i + 1)) ;;
  esac
done
ARGS+=(--image-tag "$TAG")

# How many replicas of each service run now: `up -d` without --scale would put every service back to one.
SCALE=()
while read -r n svc; do
  [ -n "${svc:-}" ] && [ "$n" -gt 1 ] && SCALE+=(--scale "$svc=$n")
done < <(docker compose ps --format '{{.Service}}' 2>/dev/null | sort | uniq -c | awk '{print $1, $2}')
echo "upgrade: to $TAG; replicas kept: ${SCALE[*]:-none above one}"

if [ "$BACKUP" = 1 ]; then
  [ -n "$BACKUP_DIR" ] || BACKUP_DIR="$STATE/backups/before-$TAG-$(date -u +%Y%m%dT%H%M%SZ)"
  mkdir -p "$(dirname "$BACKUP_DIR")"
  echo "upgrade: backup in $BACKUP_DIR"
  if [ "$KEYS" = 1 ]; then ./backup.sh --out "$BACKUP_DIR" --with-keys; else ./backup.sh --out "$BACKUP_DIR"; fi
else
  echo "upgrade: no backup (--no-backup): after a migration there is no way back"
fi

echo "upgrade: setup.sh with the arguments of the installation"
./setup.sh "${ARGS[@]}"
echo "upgrade: docker compose up -d"
docker compose up -d ${SCALE[@]+"${SCALE[@]}"}

echo "upgrade: waiting for every service to be healthy (at most ${TIMEOUT}s)"
deadline=$((SECONDS + TIMEOUT)); ok=0
while [ $SECONDS -lt $deadline ]; do
  if [ "$(docker compose ps --format '{{.Status}}' | grep -cE 'unhealthy|Restarting|starting|Exited')" = 0 ] && [ -n "$(docker compose ps --format '{{.Status}}')" ]; then ok=1; break; fi
  sleep 5
done
fail() {
  echo "upgrade: FAILED: $1" >&2
  [ "$BACKUP" = 1 ] && echo "upgrade: the way back is a restore of $BACKUP_DIR with the old images (docs/operations.md, \"Rollback\")" >&2
  exit 1
}
[ "$ok" = 1 ] || { docker compose ps >&2 || true; fail "a service is not healthy after ${TIMEOUT}s"; }
docker rm -f rusty-migrate-1 >/dev/null 2>&1 || true
status="$(docker compose run --rm --name rusty-migrate-1 migrate --config /etc/rusty/migrate.toml db status 2>&1 | tail -1)"
echo "upgrade: $status"
case "$status" in "schema: current"*) ;; *) fail "the schema is not current" ;; esac
echo "upgrade: done. Check the Monitor page and send a test message (docs/operations.md)."
[ "$BACKUP" = 0 ] || echo "upgrade: the backup of the previous state is in $BACKUP_DIR"
