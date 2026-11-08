#!/usr/bin/env bash
# Development quickstart: build and run the all-in-one server with the dev profile.
#
#   bash deploy/dev/quickstart.sh            build, then run `rusty d --dev` in the foreground
#   bash deploy/dev/quickstart.sh --smoke    build, start it on spare ports, check the banner, stop it
#                                            with SIGTERM and require exit code 0 (used by scripts/ci.sh)
#
# The dev profile keeps everything under $RUSTY_DEV_DIR (default ./rusty-dev-data, mode 0700), listens
# on 127.0.0.1 only (SMTP 2525, submission 5587, submissions 4465, IMAP 1143, IMAPS 9993) and serves
# a self-signed certificate generated in memory. It is for trying things out, never for the internet.
# Stop with Ctrl-C (exit code 0).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

cargo build -p rusty >&2
tdir="${CARGO_TARGET_DIR:-}"
if [ -z "$tdir" ]; then
  tdir="$(cargo metadata --format-version 1 --no-deps | grep -o '"target_directory":"[^"]*"' | cut -d'"' -f4)"
fi
BIN="${RUSTY_BIN:-$tdir/debug/rusty}"
[ -x "$BIN" ] || { echo "quickstart: $BIN not found" >&2; exit 1; }

if [ "${1:-}" != "--smoke" ]; then
  dir="${RUSTY_DEV_DIR:-$PWD/rusty-dev-data}"
  cat >&2 <<EOF
rusty dev profile: data in $dir
  SMTP 127.0.0.1:2525   submission 5587 (STARTTLS)   submissions 4465 (TLS)
  IMAP 127.0.0.1:1143   IMAPS 9993 (TLS)             certificate: self-signed, in memory
Until the protocol services land each port answers with a banner and closes.
EOF
  exec "$BIN" d --dev --data-dir "$dir"
fi

# ---- smoke ----
tmp="$(mktemp -d)"; chmod 700 "$tmp"
log="$tmp/log"
pid=""
cleanup() { [ -z "$pid" ] || kill -KILL "$pid" 2>/dev/null || true; rm -rf "$tmp"; }
trap cleanup EXIT
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
[ -f "$tmp/data/meta.sqlite" ] || { echo "quickstart smoke: database file missing" >&2; exit 1; }
kill -TERM "$pid"
rc=0
wait "$pid" || rc=$?
pid=""
[ "$rc" -eq 0 ] || { echo "quickstart smoke: exit code $rc after SIGTERM" >&2; cat "$log" >&2; exit 1; }
echo "quickstart smoke: OK"
