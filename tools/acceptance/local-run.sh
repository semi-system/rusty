#!/usr/bin/env bash
# First local run of Rusty (dev profile, SQLite, self-signed certificate). Run INSIDE WSL/Linux:
#   bash tools/acceptance/local-run.sh
# Starts `rusty d --dev` on a throwaway data dir, creates example.test + alice/bob, then:
#   (a) mail to alice over SMTP :2525   (b) read it over IMAPS :9993 (LOGIN, LIST, SELECT, FETCH, STORE, EXPUNGE)
#   (c) alice -> bob over submission :5587 (STARTTLS + AUTH PLAIN), bob reads it over IMAPS
# Exits non-zero on the first mismatch and always tears the daemon and the data dir down.
# Env: RUSTY_BIN (binary), BIND (default 127.0.0.1), KEEP=1 (keep the data dir and log).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIND="${BIND:-127.0.0.1}"
HOST="$BIND"; [ "$HOST" = "0.0.0.0" ] && HOST=127.0.0.1
P_SMTP=2525 P_SUB=5587 P_IMAPS=9993
DOMAIN=example.test
for t in openssl base64 timeout; do command -v "$t" >/dev/null || { echo "missing tool: $t" >&2; exit 127; }; done

BIN="${RUSTY_BIN:-}"
if [ -z "$BIN" ]; then
  for c in "${CARGO_TARGET_DIR:-$HOME/rusty-target}/x86_64-unknown-linux-musl/release/rusty" \
           "${CARGO_TARGET_DIR:-$HOME/rusty-target}/release/rusty"; do [ -x "$c" ] && BIN="$c" && break; done
fi
if [ -z "$BIN" ]; then
  echo "== building the release binary" >&2
  (cd "$ROOT" && cargo build --release --target x86_64-unknown-linux-musl -p rusty) || exit 1
  BIN="${CARGO_TARGET_DIR:-$HOME/rusty-target}/x86_64-unknown-linux-musl/release/rusty"
fi
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

WORK="$(mktemp -d "$HOME/rusty-local-run.XXXXXX")" || exit 1
DATA="$WORK/data"; LOG="$WORK/rusty.log"; PID=""
cleanup() {
  [ -n "$PID" ] && kill -TERM "$PID" 2>/dev/null && { for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$PID" 2>/dev/null || break; sleep 0.5; done; kill -KILL "$PID" 2>/dev/null; }
  if [ "${KEEP:-0}" = 1 ]; then echo "kept: $WORK" >&2; else rm -rf "$WORK"; fi
}
trap cleanup EXIT
FAILED=0
fail() { echo "FAIL: $*" >&2; echo "---- daemon log tail" >&2; tail -n 20 "$LOG" >&2; exit 1; }
expect() { # expect <label> <haystack> <needle>
  case "$2" in *"$3"*) echo "ok: $1";; *) echo "--- transcript"; printf '%s\n' "$2"; fail "$1: expected '$3'";; esac
}

ALICE_PW="$(openssl rand -hex 12)"; BOB_PW="$(openssl rand -hex 12)"
RC() { "$BIN" ctl --dev --data-dir "$DATA" "$@"; }

echo "== starting rusty d --dev (bind $BIND, data $DATA)"
"$BIN" d --dev --data-dir "$DATA" --bind "$BIND" >"$LOG" 2>&1 &
PID=$!
up=0
for _ in $(seq 1 150); do
  kill -0 "$PID" 2>/dev/null || fail "daemon exited early"
  grep -q 'service="imaps-993"' "$LOG" && { up=1; break; }
  sleep 0.2
done
[ "$up" = 1 ] || fail "daemon did not report its listeners"
sleep 0.5

echo "== provisioning"
RC domain add "$DOMAIN" || fail "domain add"
printf '%s\n' "$ALICE_PW" | RC account add "alice@$DOMAIN" --password-stdin || fail "account add alice"
printf '%s\n' "$BOB_PW" | RC account add "bob@$DOMAIN" --password-stdin || fail "account add bob"
RC account list "$DOMAIN"

# smtp_plain <port> : reads commands on stdin, one per line, lock-step; prints the transcript.
smtp_plain() {
  exec 3<>"/dev/tcp/$HOST/$1" || return 1
  local line l indata=0
  read -r -t 10 line <&3; echo "S: $line"
  while IFS= read -r line; do
    line="${line%$'\r'}"
    printf '%s\r\n' "$line" >&3; echo "C: $line"
    # inside DATA the lines are sent without waiting; the reply comes after the lone "."
    if [ "$indata" = 1 ]; then [ "$line" = "." ] || continue; indata=0; fi
    while IFS= read -r -t 10 l <&3; do l="${l%$'\r'}"; echo "S: $l"; case "$l" in [0-9][0-9][0-9]" "*|[0-9][0-9][0-9]) break;; esac; done
    case "$l" in 354*) indata=1;; esac
  done
  exec 3>&-
}
BODY1=$'From: carol@remote.test\r\nTo: alice@example.test\r\nSubject: hello-from-25\r\nDate: Mon, 10 Nov 2026 10:00:00 +0000\r\nMessage-ID: <t1@remote.test>\r\n\r\nThis is the first local run.\r\n.'
echo "== (a) SMTP inbound on port $P_SMTP"
T1="$(printf '%s\n' "EHLO client.test" "MAIL FROM:<carol@remote.test>" "RCPT TO:<alice@$DOMAIN>" "DATA" "$BODY1" "QUIT" | smtp_plain "$P_SMTP")"
printf '%s\n' "$T1"
expect "(a) message accepted" "$T1" "S: 250 2.0.0"

echo "$T1" | grep -q '^S: 5' && fail "(a) a 5xx reply"

# imap <user> <pw> <command...> : one IMAPS session; commands are sent with a pause between them.
imap() {
  local user="$1" pw="$2"; shift 2
  { sleep 1; printf 'a1 LOGIN %s %s\r\n' "$user" "$pw"; sleep 0.5
    local n=2 c; for c in "$@"; do printf 'a%s %s\r\n' "$n" "$c"; n=$((n+1)); sleep 0.5; done
    printf 'a%s LOGOUT\r\n' "$n"; sleep 0.5; } |
    timeout 30 openssl s_client -quiet -connect "$HOST:$P_IMAPS" -verify_quiet 2>/dev/null | tr -d '\r'
}
redact() { sed "s/$1/<password>/g"; }
echo "== (b) IMAPS on port $P_IMAPS as alice"
T2="$(imap "alice@$DOMAIN" "$ALICE_PW" 'LIST "" "*"' 'SELECT INBOX' 'FETCH 1 BODY[]' 'STORE 1 +FLAGS (\Deleted)' 'EXPUNGE' 'SELECT INBOX' | redact "$ALICE_PW")"
printf '%s\n' "$T2"
expect "(b) login" "$T2" "a1 OK"
expect "(b) INBOX listed" "$T2" "INBOX"
expect "(b) one message exists" "$T2" "1 EXISTS"
expect "(b) body fetched" "$T2" "This is the first local run."
expect "(b) subject fetched" "$T2" "Subject: hello-from-25"
expect "(b) store ok" "$T2" "a5 OK"
expect "(b) expunge reports" "$T2" "1 EXPUNGE"
expect "(b) mailbox empty afterwards" "$T2" "0 EXISTS"

echo "== (c) alice -> bob over submission port $P_SUB (STARTTLS + AUTH PLAIN)"
AUTH="$(printf '\0alice@%s\0%s' "$DOMAIN" "$ALICE_PW" | base64 -w0)"
{ sleep 1; printf 'EHLO client.test\n'; sleep 1; printf 'AUTH PLAIN %s\n' "$AUTH"; sleep 1
  printf 'MAIL FROM:<alice@%s>\r\nRCPT TO:<bob@%s>\r\n' "$DOMAIN" "$DOMAIN"; sleep 1
  printf 'DATA\r\n'; sleep 1
  printf 'From: alice@%s\r\nTo: bob@%s\r\nSubject: hello-from-587\r\nDate: Mon, 10 Nov 2026 10:05:00 +0000\r\nMessage-ID: <t2@%s>\r\n\r\nAlice to Bob, locally.\r\n.\r\n' "$DOMAIN" "$DOMAIN" "$DOMAIN"; sleep 1
  printf 'QUIT\r\n'; sleep 0.5; } | tr -d '' |
  timeout 30 openssl s_client -quiet -starttls smtp -crlf -connect "$HOST:$P_SUB" 2>/dev/null | tr -d '\r' | sed "s/$AUTH/<auth>/" >"$WORK/sub.txt"
cat "$WORK/sub.txt"
T3="$(cat "$WORK/sub.txt")"
expect "(c) authenticated" "$T3" "235 "
expect "(c) message accepted" "$T3" "250 "
echo "$T3" | grep -q '^5' && fail "(c) a 5xx reply"

sleep 1
echo "== (c) bob reads it over IMAPS"
T4="$(imap "bob@$DOMAIN" "$BOB_PW" 'SELECT INBOX' 'FETCH 1 BODY[]' | redact "$BOB_PW")"
printf '%s\n' "$T4"
expect "(c) bob has the mail" "$T4" "1 EXISTS"
expect "(c) subject" "$T4" "Subject: hello-from-587"
expect "(c) body" "$T4" "Alice to Bob, locally."

echo "== a wrong password is refused"
T5="$(imap "alice@$DOMAIN" "wrong-password" 'SELECT INBOX')"
expect "(d) bad login refused" "$T5" "a1 NO"

echo "== no secret in the daemon log"
if grep -qF -e "$ALICE_PW" -e "$BOB_PW" -e "$AUTH" "$LOG"; then fail "a password reached the log"; fi
echo "ok: log clean"
echo "== ALL CHECKS PASSED"
