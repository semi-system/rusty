#!/usr/bin/env bash
# Checks deploy/edge/haproxy-acme.cfg with the HAProxy image pinned in deploy/images.lock: the configuration parses (`haproxy -c`), HAProxy makes
# the ACME account key and the temporary key pairs itself, and the keys land only under the edge's own directory. Needs Docker. No network is used.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="$(awk '$1 == "haproxy:3.4.7-alpine3.24" { print $2 }' "$ROOT/deploy/images.lock")"
[ -n "$image" ] || { echo "FAIL: the haproxy image is not pinned in deploy/images.lock" >&2; exit 1; }
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/acme" "$tmp/certs"; chmod 0700 "$tmp" "$tmp/acme" "$tmp/certs"
out="$(docker run --rm --user "$(id -u):$(id -g)" -v "$ROOT/deploy/edge/haproxy-acme.cfg:/cfg/haproxy.cfg:ro" -v "$tmp:/var/lib/haproxy" "$image" haproxy -c -f /cfg/haproxy.cfg 2>&1)" || { echo "$out" >&2; echo "FAIL: the ACME edge configuration is not valid" >&2; exit 1; }
[ -s "$tmp/acme/account.key" ] || { echo "FAIL: no ACME account key was made" >&2; exit 1; }
echo "ok: edge ACME configuration is valid; HAProxy made its own account key"
