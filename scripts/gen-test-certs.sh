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

make_ca() { # name, subject
  key "$out/$1.key"
  openssl req -x509 -new -sha256 -days 7 -key "$out/$1.key" -subj "/CN=$2" -out "$out/$1.crt"
}
make_ca internal-ca "Rusty H1 Internal Test CA"
make_ca other-ca "Rusty H2 Unrelated Test CA"

sign() { # name, subject, extensions, [ca name, default internal-ca], [extra openssl x509 args...]
  local name="$1" subject="$2" extensions="$3" ca="${4:-internal-ca}"
  shift 3
  [ "$#" -gt 0 ] && shift
  key "$out/$name.key"
  openssl req -new -sha256 -key "$out/$name.key" -subj "/CN=$subject" -out "$out/$name.csr"
  printf '%s\n' "$extensions" > "$out/$name.ext"
  openssl x509 -req -sha256 -in "$out/$name.csr" \
    -CA "$out/$ca.crt" -CAkey "$out/$ca.key" -CAcreateserial \
    -extfile "$out/$name.ext" -out "$out/$name.crt" "$@" >/dev/null 2>&1
  rm -f "$out/$name.csr" "$out/$name.ext"
}

backend_ext='basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:be-imap,DNS:be-pop3,DNS:be-smtp,DNS:be-submission,DNS:be-http'
sign backend be-imap "$backend_ext" internal-ca -days 7
sign edge-client edge \
  'basicConstraints=CA:FALSE
keyUsage=digitalSignature
extendedKeyUsage=clientAuth
subjectAltName=DNS:edge' internal-ca -days 7
cat "$out/edge-client.key" "$out/edge-client.crt" > "$out/edge-client.pem"
chmod 0400 "$out/edge-client.pem"

# Deliberately bad backend certificates (H2): each must make the edge refuse the backend.
sign backend-wrongca be-imap "$backend_ext" other-ca -days 7
wrong_name_ext='basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:not-the-backend.invalid'
sign backend-wrongname not-the-backend.invalid "$wrong_name_ext" internal-ca -days 7
# CN equals the backend name but the SAN list does not (CN-fallback probe).
sign backend-cnonly be-imap "$wrong_name_ext" internal-ca -days 7
sign backend-expired be-imap "$backend_ext" internal-ca \
  -not_before 20200101000000Z -not_after 20200102000000Z

public_cert() { # name, CN, then every SAN host name
  local name="$1" cn="$2" san="" h
  shift
  for h in "$@"; do san="${san:+$san,}DNS:$h"; done
  sign "$name" "$cn" "basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=$san" internal-ca -days 7
  cat "$out/$name.key" "$out/$name.crt" > "$out/$name.pem"
  chmod 0400 "$out/$name.pem"
}
public_cert mail-a mail.test mail.test imap.test pop.test smtp.test
public_cert mail-b other.test other.test
public_cert web web.test web.test
# Replacement for mail-a used by the rotation test (same names, new key and serial).
public_cert mail-a-rotated mail.test mail.test imap.test pop.test smtp.test

chmod 0444 "$out"/*.crt
echo "Generated test-only certificates in $out"
