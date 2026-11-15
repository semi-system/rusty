#!/usr/bin/env bash
# Runs scripts/ci-web.sh in the Node image pinned by digest in deploy/images.lock (the host needs only Docker).
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="$(awk '$1 == "node:22-alpine3.22" { print $2 }' "$root/deploy/images.lock")"
[ -n "$image" ] || { echo "node:22-alpine3.22 is not pinned in deploy/images.lock" >&2; exit 1; }
exec docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -e npm_config_cache=/tmp/npm-cache \
  -v "$root:/work" -w /work "$image" sh scripts/ci-web.sh
