#!/usr/bin/env bash
# Development quickstart: build and run the all-in-one server with the dev profile.
#
#   bash deploy/dev/quickstart.sh            build, then run `rusty d --dev` in the foreground
#   bash deploy/dev/quickstart.sh --smoke    build, build the schema, start it on spare ports, check the banner,
#                                            stop it with SIGTERM and require exit code 0 (used by scripts/ci.sh)
#
# The metadata lives in PostgreSQL, the only backend. Name the server in the environment first:
#   RUSTY_DEV_PG_URL=postgres://user@host:5432/database      no password in the URL
#   RUSTY_DEV_PG_PASSWORD=...                                 optional, never logged
#   RUSTY_DEV_PG_SSLMODE / RUSTY_DEV_PG_CA_BUNDLE             optional (default: no TLS on loopback, verify-full elsewhere)
# `scripts/pg-up.sh` starts a throw-away server for this (its URL is in ~/.rusty-pg-test-url.dev, with the password
# inside: split it as `postgres://rusty@127.0.0.1:PORT/postgres` and RUSTY_DEV_PG_PASSWORD).
# The schema is built once with `rustyctl --dev db migrate --without-roles` (the smoke run does it itself).
#
# The dev profile keeps its files under $RUSTY_DEV_DIR (default ./rusty-dev-data, mode 0700), listens
# on 127.0.0.1 only (SMTP 2525, submission 5587, submissions 4465, IMAP 1143, IMAPS 9993) and serves
# a self-signed certificate. It is for trying things out, never for the internet.
# Stop with Ctrl-C (exit code 0).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

[ -n "${RUSTY_DEV_PG_URL:-}" ] || {
  echo "quickstart: set RUSTY_DEV_PG_URL (postgres://user@host:5432/database, no password) and, if the" >&2
  echo "            server wants one, RUSTY_DEV_PG_PASSWORD. See the header of this script." >&2
  exit 1
}

cargo build -p rusty >&2
tdir="${CARGO_TARGET_DIR:-}"
if [ -z "$tdir" ]; then
  tdir="$(cargo metadata --format-version 1 --no-deps | grep -o '"target_directory":"[^"]*"' | cut -d'"' -f4)"
fi
BIN="${RUSTY_BIN:-$tdir/debug/rusty}"
[ -x "$BIN" ] || { echo "quickstart: $BIN not found" >&2; exit 1; }

if [ "${1:-}" != "--smoke" ]; then
  dir="${RUSTY_DEV_DIR:-$PWD/rusty-dev-data}"
  if ! "$BIN" ctl --dev --data-dir "$dir" db status >/dev/null 2>&1; then
    echo "quickstart: the database schema is not current; build it once with:" >&2
    echo "  $BIN ctl --dev --data-dir $dir db migrate --without-roles" >&2
    exit 1
  fi
  cat >&2 <<EOF
rusty dev profile: files in $dir, database $RUSTY_DEV_PG_URL
  SMTP 127.0.0.1:2525   submission 5587 (STARTTLS)   submissions 4465 (TLS)
  IMAP 127.0.0.1:1143   IMAPS 9993 (TLS)             certificate: self-signed
EOF
  exec "$BIN" d --dev --data-dir "$dir"
fi

# ---- smoke ----
tmp="$(mktemp -d)"; chmod 700 "$tmp"
log="$tmp/log"
pid=""
cleanup() { [ -z "$pid" ] || kill -KILL "$pid" 2>/dev/null || true; rm -rf "$tmp"; }
trap cleanup EXIT
"$BIN" ctl --dev --data-dir "$tmp/data" db migrate --without-roles >/dev/null || { echo "quickstart smoke: db migrate failed" >&2; exit 1; }
# The five listeners sit at the dev ports plus an offset (SMTP 2525 ... IMAPS 9993). The offset keeps every one of them below the
# kernel's ephemeral port range (32768 and up), where an outgoing connection of this very job (Postgres, curl, the runner's own)
# may be holding the port: 10000..21999 puts them at 11143..31992. A port can still be taken by anything, so a start that cannot
# bind is retried with another offset. RUSTY_SMOKE_PORT_OFFSET is the offset of the FIRST attempt (a test occupies a port and
# checks that the retry gets past it).
banner=""
for attempt in 1 2 3 4 5; do
  if [ "$attempt" -eq 1 ] && [ -n "${RUSTY_SMOKE_PORT_OFFSET:-}" ]; then off="$RUSTY_SMOKE_PORT_OFFSET"; else off=$((10000 + RANDOM % 12000)); fi
  "$BIN" d --dev --data-dir "$tmp/data" --port-offset "$off" 2>"$log" &
  pid=$!
  banner=""
  taken=0
  for _ in $(seq 1 100); do
    if { exec 3<>"/dev/tcp/127.0.0.1/$((2525 + off))"; } 2>/dev/null; then
      read -r -t 5 banner <&3 || true
      exec 3>&- 3<&-
      # Something else may be listening on that port while our daemon failed to bind it: the daemon must still be there.
      kill -0 "$pid" 2>/dev/null || banner=""
      break
    fi
    sleep 0.1
    kill -0 "$pid" 2>/dev/null && continue
    break
  done
  if ! kill -0 "$pid" 2>/dev/null; then
    wait "$pid" 2>/dev/null || true
    pid=""
    if grep -q "cannot bind listener" "$log"; then taken=1; else echo "quickstart smoke: process exited early" >&2; cat "$log" >&2; exit 1; fi
  fi
  [ "$taken" -eq 1 ] || break
  echo "quickstart smoke: port offset $off was taken (attempt $attempt), trying another" >&2
done
case "$banner" in
  "220 "*) ;;
  *) echo "quickstart smoke: unexpected banner: $banner" >&2; cat "$log" >&2; exit 1 ;;
esac
[ -d "$tmp/data/blobs" ] || { echo "quickstart smoke: blob directory missing" >&2; exit 1; }
kill -TERM "$pid"
rc=0
wait "$pid" || rc=$?
pid=""
[ "$rc" -eq 0 ] || { echo "quickstart smoke: exit code $rc after SIGTERM" >&2; cat "$log" >&2; exit 1; }
echo "quickstart smoke: OK"
