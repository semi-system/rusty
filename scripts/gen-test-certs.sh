#!/usr/bin/env bash
# Generate throw-away H1/H2 certificates. Never use these keys outside local tests.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
umask 077

out="deploy/compose/generated"
rm -rf -- "$out"
install -d -m 0700 "$out"

key() {
  openssl genpkey -quiet -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$1"
  chmod 0400 "$1"
}

key "$out/internal-ca.key"
openssl req -x509 -new -sha256 -days 7 -key "$out/internal-ca.key" \
  -subj "/CN=Rusty H1 Internal Test CA" -out "$out/internal-ca.crt"

sign() { # name, subject, extensions
  local name="$1" subject="$2" extensions="$3"
  key "$out/$name.key"
  openssl req -new -sha256 -key "$out/$name.key" -subj "/CN=$subject" -out "$out/$name.csr"
  printf '%s\n' "$extensions" > "$out/$name.ext"
  openssl x509 -req -sha256 -days 7 -in "$out/$name.csr" \
    -CA "$out/internal-ca.crt" -CAkey "$out/internal-ca.key" -CAcreateserial \
    -extfile "$out/$name.ext" -out "$out/$name.crt" >/dev/null 2>&1
  rm -f "$out/$name.csr" "$out/$name.ext"
}

sign backend be-imap \
  'basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:be-imap,DNS:be-pop3,DNS:be-smtp,DNS:be-submission,DNS:be-http'
sign edge-client edge \
  'basicConstraints=CA:FALSE
keyUsage=digitalSignature
extendedKeyUsage=clientAuth
subjectAltName=DNS:edge'
cat "$out/edge-client.key" "$out/edge-client.crt" > "$out/edge-client.pem"
chmod 0400 "$out/edge-client.pem"

public_cert() { # name, CN, then every SAN host name
  local name="$1" cn="$2" san="" h
  shift
  for h in "$@"; do san="${san:+$san,}DNS:$h"; done
  sign "$name" "$cn" "basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=$san"
  cat "$out/$name.key" "$out/$name.crt" > "$out/$name.pem"
  chmod 0400 "$out/$name.pem"
}
public_cert mail-a mail.test mail.test imap.test pop.test smtp.test
public_cert mail-b other.test other.test
public_cert web web.test web.test

chmod 0444 "$out"/*.crt
echo "Generated test-only certificates in $out"
