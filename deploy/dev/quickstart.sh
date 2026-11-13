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
off=$((20000 + RANDOM % 20000))
"$BIN" d --dev --data-dir "$tmp/data" --port-offset "$off" 2>"$log" &
pid=$!
banner=""
for _ in $(seq 1 100); do
  if { exec 3<>"/dev/tcp/127.0.0.1/$((2525 + off))"; } 2>/dev/null; then
    read -r -t 5 banner <&3 || true
    exec 3>&- 3<&-
    break
  fi
  kill -0 "$pid" 2>/dev/null || { echo "quickstart smoke: process exited early" >&2; cat "$log" >&2; exit 1; }
  sleep 0.1
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
