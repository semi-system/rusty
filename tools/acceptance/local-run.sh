#!/usr/bin/env bash
# First local run of Rusty (dev profile, self-signed certificate, PostgreSQL). Run INSIDE WSL/Linux with Docker:
#   bash tools/acceptance/local-run.sh
# Starts a throw-away Postgres 18 container (image pinned in deploy/images.lock, TLS with a generated CA, the
# connection verified with verify-full) initialised with deploy/db/roles.sql, builds the schema as rusty_migrator,
# starts `rusty d --dev` as rusty_app (DML only) on a throwaway data dir, creates example.test + alice/bob, then:
#   (a) mail to alice over SMTP :2525   (b) read it over IMAPS :9993 (LOGIN, LIST, SELECT, FETCH, STORE, EXPUNGE)
#   (c) alice -> bob over submission :5587 (STARTTLS + AUTH PLAIN), bob reads it over IMAPS
# It also checks that the daemon refuses an un-migrated database and (binary mode) kills the daemon with SIGKILL in
# the middle of a burst of deliveries: after the restart and the senders' retries every message exists once.
# Exits non-zero on the first mismatch and always tears the daemon, the database and the data dir down.
# The dev profile names its database in the environment (RUSTY_DEV_PG_URL, RUSTY_DEV_PG_PASSWORD,
# RUSTY_DEV_PG_SSLMODE, RUSTY_DEV_PG_CA_BUNDLE); this script sets them for every rusty command it runs.
# Env: RUSTY_BIN (binary), BIND (default 127.0.0.1), KEEP=1 (keep the data dir and log),
#      IMAGE=<ref> run the daemon from a container image built with --build-arg DEV_CERT=1 instead
#      (read-only root, all capabilities dropped, host network so that it reaches the database on 127.0.0.1).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIND="${BIND:-127.0.0.1}"
HOST="$BIND"; [ "$HOST" = "0.0.0.0" ] && HOST=127.0.0.1
P_SMTP=2525 P_SUB=5587 P_IMAPS=9993
DOMAIN=example.test
for t in openssl base64 timeout docker; do command -v "$t" >/dev/null || { echo "missing tool: $t" >&2; exit 127; }; done

IMAGE="${IMAGE:-}"
CNAME="rusty-local-run-$$"
BIN="${RUSTY_BIN:-}"
if [ -n "$IMAGE" ]; then
  HOST=127.0.0.1
else
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
fi

WORK="$(mktemp -d "$HOME/rusty-local-run.XXXXXX")" || exit 1
DATA="$WORK/data"; LOG="$WORK/rusty.log"; PID=""
DATADIR="$DATA"; [ -n "$IMAGE" ] && DATADIR=/var/lib/rusty/dev
PGC=""
cleanup() {
  [ -n "$PGC" ] && docker rm -f "$PGC" >/dev/null 2>&1
  if [ -n "$IMAGE" ]; then docker rm -f "$CNAME" >/dev/null 2>&1; docker volume rm -f "$CNAME-data" >/dev/null 2>&1; fi
  [ -n "$PID" ] && kill -TERM "$PID" 2>/dev/null && { for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$PID" 2>/dev/null || break; sleep 0.5; done; kill -KILL "$PID" 2>/dev/null; }
  if [ "${KEEP:-0}" = 1 ]; then echo "kept: $WORK" >&2; else rm -rf "$WORK"; fi
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; echo "---- daemon log tail" >&2; tail -n 20 "$LOG" >&2; exit 1; }
expect() { # expect <label> <haystack> <needle>
  case "$2" in *"$3"*) echo "ok: $1";; *) echo "--- transcript"; printf '%s\n' "$2"; fail "$1: expected '$3'";; esac
}

ALICE_PW="$(openssl rand -hex 12)"; BOB_PW="$(openssl rand -hex 12)"

# rusty_as <role> <password> <args...>: the rusty binary (or the image) with the dev database environment: the
# role logs in over TLS and verifies the server certificate against the generated CA.
rusty_as() {
  local role="$1" pw="$2"; shift 2
  local url="postgres://$role@127.0.0.1:$PGPORT_HOST/rusty"
  if [ -n "$IMAGE" ]; then
    docker run --rm ${DOCKER_I:-} --network host --read-only --cap-drop ALL --security-opt no-new-privileges \
      -v "$CNAME-data:/var/lib/rusty" -v "$WORK/secrets:/run/pg-secrets:ro" \
      -e RUSTY_DEV_PG_URL="$url" -e RUSTY_DEV_PG_PASSWORD="$pw" -e RUSTY_DEV_PG_SSLMODE=verify-full \
      -e RUSTY_DEV_PG_CA_BUNDLE=/run/pg-secrets/ca.pem "$IMAGE" "$@"
  else
    # RUSTY_EXEC=1 (set only on a backgrounded call) replaces the subshell with the binary, so that `$!` is the
    # daemon itself and kill -KILL reaches it.
    local run=(env RUSTY_DEV_PG_URL="$url" RUSTY_DEV_PG_PASSWORD="$pw" RUSTY_DEV_PG_SSLMODE=verify-full
      RUSTY_DEV_PG_CA_BUNDLE="$WORK/secrets/ca.pem" "$BIN" "$@")
    if [ -n "${RUSTY_EXEC:-}" ]; then exec "${run[@]}"; else "${run[@]}"; fi
  fi
}
RC() { DOCKER_I=-i rusty_as rusty_app "$APP_PW" ctl --dev --data-dir "$DATADIR" "$@"; }

start_daemon() {
  if [ -n "$IMAGE" ]; then
    echo "== starting $IMAGE (read-only root, no capabilities, host network)"
    docker run -d --name "$CNAME" --network host --read-only --cap-drop ALL --security-opt no-new-privileges \
      -v "$CNAME-data:/var/lib/rusty" -v "$WORK/secrets:/run/pg-secrets:ro" \
      -e RUSTY_DEV_PG_URL="postgres://rusty_app@127.0.0.1:$PGPORT_HOST/rusty" -e RUSTY_DEV_PG_PASSWORD="$APP_PW" \
      -e RUSTY_DEV_PG_SSLMODE=verify-full -e RUSTY_DEV_PG_CA_BUNDLE=/run/pg-secrets/ca.pem \
      "$IMAGE" d --dev --data-dir "$DATADIR" >/dev/null || exit 1
    docker logs -f "$CNAME" >"$LOG" 2>&1 &
  else
    RUSTY_EXEC=1 rusty_as rusty_app "$APP_PW" d --dev --data-dir "$DATADIR" --bind "$BIND" >>"$LOG" 2>&1 &
  fi
  PID=$!
  local up=0 _
  for _ in $(seq 1 150); do
    kill -0 "$PID" 2>/dev/null || fail "daemon exited early"
    [ "$(grep -c 'service="imaps-993"' "$LOG")" -ge "${LISTENING:-1}" ] && { up=1; break; }
    sleep 0.2
  done
  [ "$up" = 1 ] || fail "daemon did not report its listeners"
  sleep 0.5
}

echo "== postgres: container with TLS, roles.sql, migrator"
[ -n "$IMAGE" ] && { docker volume create "$CNAME-data" >/dev/null || exit 1; }
IMAGE_REF="$(awk '$1 == "postgres:18" { print $2 }' "$ROOT/deploy/images.lock")"
[ -n "$IMAGE_REF" ] || fail "no postgres image in deploy/images.lock"
TLS="$WORK/pgtls"; mkdir -p "$TLS"; chmod 755 "$TLS"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$TLS/ca.key" -out "$TLS/ca.pem" -days 2 -subj "/CN=rusty-acceptance-ca" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null || fail "ca"
openssl req -newkey rsa:2048 -nodes -keyout "$TLS/server.key" -out "$TLS/server.csr" -subj "/CN=localhost" 2>/dev/null || fail "csr"
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\n' >"$TLS/ext.cnf"
openssl x509 -req -in "$TLS/server.csr" -CA "$TLS/ca.pem" -CAkey "$TLS/ca.key" -CAcreateserial -out "$TLS/server.pem" -days 2 -extfile "$TLS/ext.cnf" 2>/dev/null || fail "server cert"
chmod 644 "$TLS"/*
SUPER_PW="$(openssl rand -hex 16)"; APP_PW="$(openssl rand -hex 16)"; MIG_PW="$(openssl rand -hex 16)"
PGC="rusty-acc-pg-$$"
# The key must belong to the server user with mode 0600: copy it into place before starting.
docker run -d --name "$PGC" -e POSTGRES_USER=rusty -e POSTGRES_DB=rusty -e POSTGRES_PASSWORD="$SUPER_PW" \
  -p 127.0.0.1::5432 -v "$TLS:/certs:ro" --user 999:999 --tmpfs /var/lib/postgresql:uid=999,gid=999,mode=0700,size=1g \
  --tmpfs /var/run/postgresql:uid=999,gid=999,mode=0755 --tmpfs /ssl:uid=999,gid=999,mode=0700 --cap-drop ALL \
  --security-opt no-new-privileges:true --entrypoint sh "$IMAGE_REF" -c \
  'cp /certs/server.pem /certs/server.key /ssl/ && chmod 600 /ssl/server.key && exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/ssl/server.pem -c ssl_key_file=/ssl/server.key -c listen_addresses=* -c fsync=off' >/dev/null || fail "start postgres"
for _ in $(seq 1 60); do docker exec "$PGC" pg_isready -h 127.0.0.1 -U rusty -d rusty >/dev/null 2>&1 && break; sleep 1; done
docker exec "$PGC" pg_isready -h 127.0.0.1 -U rusty -d rusty >/dev/null 2>&1 || fail "postgres did not become ready"
sleep 2
PGPORT_HOST="$(docker port "$PGC" 5432/tcp | head -n1 | sed 's/.*://')"
docker exec -i "$PGC" psql -h 127.0.0.1 -U rusty -d rusty -v ON_ERROR_STOP=1 -q <"$ROOT/deploy/db/roles.sql" >/dev/null || fail "roles.sql"
printf "ALTER ROLE rusty_app LOGIN PASSWORD '%s'; ALTER ROLE rusty_migrator LOGIN PASSWORD '%s';\n" "$APP_PW" "$MIG_PW" |
  docker exec -i "$PGC" psql -h 127.0.0.1 -U rusty -d rusty -v ON_ERROR_STOP=1 -q >/dev/null || fail "role logins"
umask 077
mkdir -p "$WORK/secrets"
cp "$TLS/ca.pem" "$WORK/secrets/ca.pem"
umask 022
chmod 755 "$WORK/secrets"; chmod 644 "$WORK/secrets/ca.pem"

echo "== postgres: the daemon refuses an un-migrated database"
RUSTY_EXEC=1 rusty_as rusty_app "$APP_PW" d --dev --data-dir "$DATADIR" --bind "$BIND" >"$LOG" 2>&1 &
PID=$!
for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
if kill -0 "$PID" 2>/dev/null; then fail "the daemon started on an un-migrated database"; fi
wait "$PID"; RC_EXIT=$?; PID=""
[ "$RC_EXIT" != 0 ] || fail "the daemon exited 0 on an un-migrated database"
grep -q "not initialised" "$LOG" || fail "no clear error about the missing schema"
echo "ok: refused (exit $RC_EXIT): $(grep -m1 'not initialised' "$LOG")"
echo "== postgres: rustyctl db status / migrate (migrator role)"
rusty_as rusty_migrator "$MIG_PW" ctl --dev --data-dir "$DATADIR" db status && fail "db status said current before migrating"
rusty_as rusty_migrator "$MIG_PW" ctl --dev --data-dir "$DATADIR" db migrate || fail "db migrate"
rusty_as rusty_migrator "$MIG_PW" ctl --dev --data-dir "$DATADIR" db migrate | grep -q "newly applied \[\]" || fail "a second migrate was not a no-op"
rusty_as rusty_migrator "$MIG_PW" ctl --dev --data-dir "$DATADIR" db status || fail "db status after migrate"
: >"$LOG"

echo "== starting rusty d --dev (bind $BIND, data $DATADIR)"
start_daemon

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

if [ -z "$IMAGE" ]; then
  echo "== postgres: kill -9 in the middle of a burst of deliveries (no loss, no duplicate)"
  N=300
  : >"$WORK/acked"
  burst_body() { printf 'From: carol@remote.test
To: alice@example.test
Subject: burst-%s
Date: Mon, 10 Nov 2026 11:00:00 +0000
Message-ID: <b%s@remote.test>

burst %s
.' "$1" "$1" "$1"; }
  send_burst() { # n : prints n when the server answered 250 to the end of DATA
    local t
    t="$(printf '%s
' "EHLO client.test" "MAIL FROM:<carol@remote.test>" "RCPT TO:<alice@$DOMAIN>" "DATA" "$(burst_body "$1")" "QUIT" | smtp_plain "$P_SMTP" 2>/dev/null)"
    case "$t" in *"S: 250 2.0.0"*) echo "$1" >>"$WORK/acked";; esac
  }
  ( for i in $(seq 1 $N); do send_burst "$i"; done ) >/dev/null 2>&1 &
  BURST=$!
  for _ in $(seq 1 200); do [ "$(wc -l <"$WORK/acked")" -ge 10 ] && break; sleep 0.05; done
  kill -KILL "$PID"; wait "$PID" 2>/dev/null; PID=""
  ACKED_AT_KILL="$(wc -l <"$WORK/acked")"
  wait "$BURST" 2>/dev/null
  [ "$ACKED_AT_KILL" -lt "$N" ] || fail "the burst finished before the daemon was killed (test is vacuous)"
  echo "ok: daemon killed with SIGKILL after $ACKED_AT_KILL of $N acknowledged messages"
  LISTENING=2 start_daemon
  sort -n "$WORK/acked" >"$WORK/acked.sorted"
  retried=0
  for i in $(seq 1 $N); do
    grep -qx "$i" "$WORK/acked.sorted" && continue
    retried=$((retried+1)); send_burst "$i"
  done
  echo "ok: $retried messages were not acknowledged before the kill and were sent again"
  [ "$(sort -u "$WORK/acked" | wc -l)" -eq "$N" ] || fail "not every message was acknowledged after the retry"
  T6="$(imap "alice@$DOMAIN" "$ALICE_PW" 'SELECT INBOX' 'FETCH 1:* (BODY.PEEK[HEADER.FIELDS (SUBJECT)])' | redact "$ALICE_PW")"
  total="$(printf '%s
' "$T6" | grep -c '^Subject: burst-')"
  uniq_n="$(printf '%s
' "$T6" | grep '^Subject: burst-' | sort -u | wc -l)"
  [ "$total" = "$N" ] || fail "expected $N messages after the kill and the retries, found $total (loss or duplicate)"
  [ "$uniq_n" = "$N" ] || fail "duplicates after the kill and the retries: $total messages, $uniq_n distinct"
  echo "ok: $N messages, $uniq_n distinct: none lost, none duplicated"
fi


echo "== no secret in the daemon log"
if grep -qF -e "$APP_PW" -e "$MIG_PW" -e "$SUPER_PW" "$LOG"; then fail "a database password reached the log"; fi
if grep -qF -e "$ALICE_PW" -e "$BOB_PW" -e "$AUTH" "$LOG"; then fail "a password reached the log"; fi
echo "ok: log clean"
echo "== ALL CHECKS PASSED"
