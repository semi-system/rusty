#!/usr/bin/env bash
# Real ACME HTTP-01 issuance and renewal by HAProxy 3.4's own client against Pebble (the Let's Encrypt test CA), on a private Docker network. Run by the
# nightly edge job: `bash scripts/tests/acme-pebble.sh`. Needs Docker and openssl. Images are pinned by digest in deploy/images.lock.
#
# What it proves: HAProxy makes the account key, registers, answers the HTTP-01 challenge from its ACME map on the challenge port, installs the certificate
# (valid for the requested name, issued by the CA, not its temporary self-signed one) with no reload, and renews on request (`acme renew`) swapping in a
# new certificate while the listener keeps answering.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
pin() { awk -v n="$1" '$1 == n { print $2 }' "$ROOT/deploy/images.lock"; }
HAPROXY="$(pin haproxy:3.4.7-alpine3.24)"; OPENSSL_IMG="$(pin postgres:18)"; PEBBLE="$(pin ghcr.io/letsencrypt/pebble:latest)"; CHALL="$(pin ghcr.io/letsencrypt/pebble-challtestsrv:latest)"
[ -n "$HAPROXY" ] && [ -n "$PEBBLE" ] && [ -n "$CHALL" ] || { echo "acme-pebble: an image is not pinned in deploy/images.lock" >&2; exit 1; }

id="rusty-acme-$$"; net="$id-net"; tmp="$(mktemp -d)"
NAME=mail.example.test
cleanup() {
  [ "${KEEP:-0}" = 1 ] || { docker rm -f "$id-ha" "$id-pebble" "$id-chall" >/dev/null 2>&1 || true; docker network rm "$net" >/dev/null 2>&1 || true; rm -rf "$tmp"; }
}
trap cleanup EXIT
fail() { echo "acme-pebble: $*" >&2; docker logs "$id-ha" 2>&1 | tail -25 >&2 || true; exit 1; }

docker network create --subnet 10.30.50.0/24 "$net" >/dev/null
# Pebble resolves the name through the challenge test server, which answers every name with HAProxy's address; its own challenge responders are off.
docker run -d --name "$id-chall" --network "$net" --ip 10.30.50.3 "$CHALL" -defaultIPv4 10.30.50.4 -defaultIPv6 "" -http01 "" -https01 "" -tlsalpn01 "" >/dev/null
cat > "$tmp/pebble.json" <<'JSON'
{ "pebble": { "listenAddress": "0.0.0.0:14000", "managementListenAddress": "0.0.0.0:15000", "certificate": "test/certs/localhost/cert.pem", "privateKey": "test/certs/localhost/key.pem", "httpPort": 5002, "tlsPort": 5001, "ocspResponderURL": "", "externalAccountBindingRequired": false } }
JSON
docker run -d --name "$id-pebble" --network "$net" --ip 10.30.50.2 -e PEBBLE_VA_NOSLEEP=1 -v "$tmp/pebble.json:/test/config/pebble-config.json:ro" \
  "$PEBBLE" -config /test/config/pebble-config.json -dnsserver 10.30.50.3:8053 >/dev/null

mkdir -p "$tmp/data/acme" "$tmp/data/certs"; chmod -R 0777 "$tmp/data"
cat > "$tmp/haproxy.cfg" <<CFG
global
    expose-experimental-directives
    log stdout format raw local0
    httpclient.ssl.verify none
    stats socket ipv4@0.0.0.0:9999 level admin

defaults
    mode tcp
    timeout connect 3s
    timeout client 10s
    timeout server 10s

acme pebble
    directory https://10.30.50.2:14000/dir
    contact admin@example.test
    challenge HTTP-01
    keytype ECDSA
    account-key /var/lib/haproxy/acme/account.key
    map virt@acme

crt-store certs
    crt-base /var/lib/haproxy/certs
    key-base /var/lib/haproxy/certs
    load crt "mail.pem" acme pebble domains "$NAME"

# Pebble validates HTTP-01 on port 5002.
frontend challenge
    mode http
    bind :5002
    http-request return status 200 content-type text/plain lf-string "%[path,field(-1,/)].%[path,field(-1,/),map(virt@acme)]" if { path_beg /.well-known/acme-challenge/ }
    http-request return status 404

frontend imaps
    bind :8993 ssl crt "@certs/mail.pem"
    default_backend nowhere

backend nowhere
    server none 127.0.0.1:1 disabled
CFG
docker run -d --name "$id-ha" --network "$net" --ip 10.30.50.4 -v "$tmp/haproxy.cfg:/usr/local/etc/haproxy/haproxy.cfg:ro" -v "$tmp/data:/var/lib/haproxy" \
  "$HAPROXY" >/dev/null

# The certificate the listener presents: (issuer, serial) of what `openssl s_client` receives.
presented() {
  docker run --rm --network "$net" --entrypoint sh "$OPENSSL_IMG" -c "echo | openssl s_client -connect 10.30.50.4:8993 -servername $NAME 2>/dev/null | openssl x509 -noout -issuer -serial -subject -ext subjectAltName 2>/dev/null" 2>/dev/null || true
}
first=""
for _ in $(seq 1 90); do
  out="$(presented)"
  if grep -qi "pebble" <<<"$out"; then first="$out"; break; fi
  sleep 2
done
[ -n "$first" ] || fail "no certificate from Pebble was installed within three minutes"
grep -q "$NAME" <<<"$first" || fail "the certificate does not name $NAME: $first"
echo "ok: issued by Pebble for $NAME"
serial1="$(sed -n 's/^serial=//p' <<<"$first")"

# Renewal on request, over the runtime API (a TCP socket in this test configuration only): a new certificate (another serial) replaces the old one
# while the listener keeps answering.
runtime() { docker run --rm --network "$net" --entrypoint bash "$OPENSSL_IMG" -c "exec 3<>/dev/tcp/10.30.50.4/9999; echo '$1' >&3; timeout 3 cat <&3 || true"; }
resp=""
for name in '@certs/mail.pem' /var/lib/haproxy/certs/mail.pem certs/mail.pem; do
  resp="$(runtime "acme renew $name" 2>&1)" || true
  case "$resp" in *"find the certificate"*) continue ;; *) break ;; esac
done
echo "runtime: $(tr '\n' ' ' <<<"$resp" | cut -c1-200)"
renewed=""
for _ in $(seq 1 60); do
  out="$(presented)"
  s="$(sed -n 's/^serial=//p' <<<"$out")"
  if [ -n "$s" ] && [ "$s" != "$serial1" ]; then renewed="$out"; break; fi
  sleep 2
done
[ -n "$renewed" ] || fail "the certificate was not renewed (serial stayed $serial1)"
echo "ok: renewed without a reload (serial $serial1 -> $(sed -n 's/^serial=//p' <<<"$renewed"))"
echo "acme-pebble OK"
