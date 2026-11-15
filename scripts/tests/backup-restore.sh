#!/usr/bin/env bash
# Script test of scripts/backup.sh and scripts/restore.sh against a throw-away Postgres (the image pinned in deploy/images.lock): data and blobs survive
# a backup, a drop and a restore byte for byte; the database is dumped BEFORE the blobs are copied; and every refusal leaves the target untouched
# (a non-empty out directory, a damaged backup, a non-empty database, a non-empty blob directory, a directory that is not a blob root). Needs Docker.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
image="$(awk '$1 == "postgres:18" { print $2 }' "$ROOT/deploy/images.lock")"
[ -n "$image" ] || { echo "backup-restore test: postgres is not pinned in deploy/images.lock" >&2; exit 1; }
name="rusty-br-test-$$"
tmp="$(mktemp -d)"
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; rm -rf "$tmp"; }
trap cleanup EXIT
fail() { echo "backup-restore test: $*" >&2; exit 1; }

docker run -d --name "$name" -e POSTGRES_PASSWORD=test -e POSTGRES_USER=rusty "$image" >/dev/null
for _ in $(seq 1 60); do docker exec "$name" pg_isready -h 127.0.0.1 -U rusty >/dev/null 2>&1 && break; sleep 1; done
docker exec "$name" pg_isready -h 127.0.0.1 -U rusty >/dev/null 2>&1 || fail "postgres did not start"
sql() { docker exec -i "$name" psql -h 127.0.0.1 -U rusty -v ON_ERROR_STOP=1 -At "$@"; }
sql -d postgres -c "CREATE DATABASE src" -c "CREATE DATABASE dst" >/dev/null
sql -d src -c "CREATE TABLE messages (id int PRIMARY KEY, body text)" -c "INSERT INTO messages SELECT g, 'message ' || g FROM generate_series(1, 500) g" >/dev/null

export PG_DUMP="docker exec -i $name pg_dump -h 127.0.0.1 -U rusty -d src"
export PG_RESTORE="docker exec -i $name pg_restore -h 127.0.0.1 -U rusty -d dst"
export PSQL="docker exec -i $name psql -h 127.0.0.1 -U rusty -d dst"

# A blob root with some files in sharded directories (what FsBlobStore writes).
blobs="$tmp/blobs"; mkdir -p "$blobs/ab" "$blobs/cd"; echo marker > "$blobs/.rusty-blobs"
head -c 4096 /dev/urandom > "$blobs/ab/abcd"; head -c 9000 /dev/urandom > "$blobs/cd/cdef"
(cd "$blobs" && find . -type f | LC_ALL=C sort | xargs sha256sum) > "$tmp/blobs.sum"

# Backup.
bash "$ROOT/scripts/backup.sh" --out "$tmp/bk" --blobs "$blobs" >/dev/null
[ "$(stat -c %a "$tmp/bk")" = 700 ] || fail "the backup directory is not 0700"
db_done="$(awk '$1 == "database-dump-done" { print $2 }' "$tmp/bk/MANIFEST")"
blobs_started="$(awk '$1 == "blobs-copy-started" { print $2 }' "$tmp/bk/MANIFEST")"
[[ "$db_done" < "$blobs_started" || "$db_done" == "$blobs_started" ]] || fail "the blobs were copied before the dump was finished"
grep -q "^order database-first-then-blobs" "$tmp/bk/MANIFEST" || fail "the manifest does not state the order"
[ -s "$tmp/bk/SHA256SUMS" ] || fail "no checksums"

# Refusals of backup.
bash "$ROOT/scripts/backup.sh" --out "$tmp/bk" --blobs "$blobs" >/dev/null 2>&1 && fail "a backup into a non-empty directory was accepted"
mkdir "$tmp/notroot"
bash "$ROOT/scripts/backup.sh" --out "$tmp/bk2" --blobs "$tmp/notroot" >/dev/null 2>&1 && fail "a directory that is not a blob root was backed up"
[ ! -e "$tmp/bk2" ] || fail "a refused backup left a directory behind"

# Refusals of restore leave every target untouched.
cp -a "$tmp/bk" "$tmp/bad"; echo tampered >> "$tmp/bad/db.dump"
mkdir "$tmp/target-bad"
bash "$ROOT/scripts/restore.sh" --from "$tmp/bad" --blobs "$tmp/target-bad" >/dev/null 2>&1 && fail "a damaged backup was restored"
[ -z "$(ls -A "$tmp/target-bad")" ] || fail "a refused restore wrote blobs"
[ "$(sql -d dst -c "SELECT count(*) FROM pg_tables WHERE schemaname = 'public'")" = 0 ] || fail "a refused restore wrote to the database"
mkdir "$tmp/full"; echo x > "$tmp/full/file"
bash "$ROOT/scripts/restore.sh" --from "$tmp/bk" --blobs "$tmp/full" >/dev/null 2>&1 && fail "a restore into a non-empty blob directory was accepted"
[ "$(sql -d dst -c "SELECT count(*) FROM pg_tables WHERE schemaname = 'public'")" = 0 ] || fail "the database was written before the blob directory was checked"
sql -d dst -c "CREATE TABLE leftover (x int)" >/dev/null
bash "$ROOT/scripts/restore.sh" --from "$tmp/bk" --blobs "$tmp/restored" >/dev/null 2>&1 && fail "a restore into a non-empty database was accepted"
[ ! -e "$tmp/restored" ] || fail "a refused restore created the blob directory"
sql -d dst -c "DROP TABLE leftover" >/dev/null

# The real restore: data and files come back identical.
bash "$ROOT/scripts/restore.sh" --from "$tmp/bk" --blobs "$tmp/restored" >/dev/null || fail "the restore failed"
[ "$(sql -d dst -c "SELECT count(*), sum(id) FROM messages")" = "500|125250" ] || fail "the restored table differs"
[ "$(sql -d dst -c "SELECT body FROM messages WHERE id = 321")" = "message 321" ] || fail "a restored row differs"
(cd "$tmp/restored" && find . -type f | LC_ALL=C sort | xargs sha256sum) > "$tmp/restored.sum"
diff "$tmp/blobs.sum" "$tmp/restored.sum" >/dev/null || fail "the restored blob files differ"
[ "$(stat -c %a "$tmp/restored")" = 700 ] || fail "the restored blob directory is not 0700"
echo "backup-restore self-test OK"
