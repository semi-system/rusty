#!/bin/sh
# Copies the certificate HAProxy obtained over ACME to files the mail services read (tls.cert_file / tls.key_file), for the STARTTLS ports (25, 587, 143)
# that the edge passes through. HAProxy keeps its certificate in memory only; this asks its runtime socket for it (`dump ssl cert`) and writes
#   <outdir>/cert.pem   the certificate chain, mode 0644
#   <outdir>/key.pem    the private key, mode 0600, owned by whoever runs this script (run it as the mail services' uid)
# atomically (a new file renamed over the old one), and only when the content changed: the services reload on a change.
#
#   cert-sync.sh <runtime socket> <certificate, e.g. @certs/mail.pem> <outdir> [interval seconds: loop forever]
set -eu
sock="${1:?runtime socket}"; name="${2:?certificate name}"; out="${3:?output directory}"; every="${4:-0}"
umask 077

sync_once() {
  tmp="$(mktemp -d "$out/.sync.XXXXXX")"
  trap 'rm -rf "$tmp"' EXIT
  echo "dump ssl cert $name" | socat -t5 - "UNIX-CONNECT:$sock" > "$tmp/all.pem" || return 1
  awk -v k="$tmp/key.pem" -v c="$tmp/cert.pem" '
    /^-----BEGIN / { dest = ($0 ~ /PRIVATE KEY/) ? k : c }
    dest != "" { print > dest }
    /^-----END /  { dest = "" }' "$tmp/all.pem"
  [ -s "$tmp/key.pem" ] && [ -s "$tmp/cert.pem" ] || { echo "cert-sync: HAProxy returned no certificate for $name" >&2; return 1; }
  chmod 0644 "$tmp/cert.pem"; chmod 0600 "$tmp/key.pem"
  if [ -f "$out/cert.pem" ] && [ -f "$out/key.pem" ] && cmp -s "$tmp/cert.pem" "$out/cert.pem" && cmp -s "$tmp/key.pem" "$out/key.pem"; then
    rm -rf "$tmp"; trap - EXIT; return 0
  fi
  # The key first: a reader that sees the new certificate then also finds its key; one that sees the new key with the old certificate fails to load
  # the pair and keeps what it has until the certificate lands (a moment later).
  mv "$tmp/key.pem" "$out/key.pem"; mv "$tmp/cert.pem" "$out/cert.pem"
  rm -rf "$tmp"; trap - EXIT
  echo "cert-sync: wrote $out/cert.pem and key.pem"
}

if [ "$every" -gt 0 ] 2>/dev/null; then
  while :; do sync_once || echo "cert-sync: failed, will retry" >&2; sleep "$every"; done
else
  sync_once
fi
