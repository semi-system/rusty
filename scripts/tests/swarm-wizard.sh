#!/usr/bin/env bash
# The questions of `rusty-swarm init` (deploy/swarm/wizard.sh), answered on the standard input in the plain mode: what is written, the modes of the files, that a password is never in swarm.env,
# that a bad answer is asked again, and that a cancelled wizard writes nothing.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
tool="$PWD/deploy/swarm/rusty-swarm"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
export HOME="$tmp/home"; mkdir -p "$HOME"; export RUSTY_SWARM_UI=plain
fail() { echo "swarm-wizard: $*" >&2; exit 1; }

# 1. This machine, an external PostgreSQL (TLS on) and an external S3.
env1="$tmp/one.env"
printf '%s\n' 1 203.0.113.10 mail.example.org admin.example.org ops@example.org "203.0.113.5/32 198.51.100.0/24" \
  1 db.example.org "" "" "" 's3cr3t-db' "" \
  1 https://s3.example.org "" "" AKIA123 's3cr3t-s3' | "$tool" --env "$env1" init >/dev/null 2>&1 || true
[ -s "$env1" ] || fail "no swarm.env was written"
[ "$(stat -c %a "$env1")" = 600 ] || fail "swarm.env is not mode 0600"
grep -q 's3cr3t' "$env1" && fail "a password is in swarm.env"
grep -q '^DB_HOST=db.example.org' "$env1" || fail "DB_HOST is missing"
grep -q '^DB_NAME=rusty' "$env1" || fail "the default database name is not rusty"
grep -q '^DB_ADMIN_USER=postgres' "$env1" || fail "the default login is not postgres"
grep -q '^DB_TLS=1' "$env1" || fail "TLS is not on by default"
grep -q '^S3_BUCKET=rusty-mail' "$env1" || fail "the default bucket is missing"
[ "$(cat "$HOME/.rusty-swarm/db-admin.pw")" = 's3cr3t-db' ] || fail "the database password file is wrong"
[ "$(cat "$HOME/.rusty-swarm/s3-secret")" = 's3cr3t-s3' ] || fail "the S3 secret file is wrong"
[ "$(stat -c %a "$HOME/.rusty-swarm/db-admin.pw")" = 600 ] || fail "the password file is not mode 0600"
out="$("$tool" --env "$env1" inventory)"
case "$out" in *"server  local  manager"*"PostgreSQL: external db.example.org:5432/rusty"*"S3: external https://s3.example.org"*) ;; *) fail "the inventory is not understood: $out" ;; esac

# 2. Defaults in the cluster for both, and a bad answer that is asked again (a host name with a space, then a good one).
env2="$tmp/two.env"
rm -rf "$HOME/.rusty-swarm"
printf '%s\n' 1 203.0.113.10 "bad host" mail.example.org admin.example.org ops@example.org "203.0.113.5" 2 2 | "$tool" --env "$env2" init >/dev/null 2>&1 || true
[ -s "$env2" ] || fail "the second wizard wrote nothing"
out="$("$tool" --env "$env2" inventory)"
case "$out" in *"mail host: mail.example.org"*"PostgreSQL: the default one"*"S3: the default RustFS"*) ;; *) fail "the defaults are not understood: $out" ;; esac
[ ! -e "$HOME/.rusty-swarm/db-admin.pw" ] || fail "a password file was made for the default database"

# 3. The admin web must not be opened to everyone.
env3="$tmp/three.env"
if printf '%s\n' 1 203.0.113.10 mail.example.org admin.example.org ops@example.org "0.0.0.0/0" | "$tool" --env "$env3" init >/dev/null 2>&1; then fail "0.0.0.0/0 was accepted"; fi
[ ! -e "$env3" ] || fail "a refused wizard left a file"

# 4. A cancelled wizard (the input ends) writes nothing.
env4="$tmp/four.env"
if printf '%s\n' 1 203.0.113.10 | "$tool" --env "$env4" init >/dev/null 2>&1; then fail "a cancelled wizard went on"; fi
[ ! -e "$env4" ] || fail "a cancelled wizard wrote a file"

# 5. Without a terminal and without the plain mode, nothing is asked: the missing inventory is reported.
if RUSTY_SWARM_UI="" "$tool" --env "$tmp/none.env" init </dev/null >/dev/null 2>&1; then fail "init without an inventory and without a terminal went on"; fi
echo "swarm-wizard OK"
