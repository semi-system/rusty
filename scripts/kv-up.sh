#!/usr/bin/env bash
# Start the throw-away test Valkey (deploy/dev/valkey.compose.yaml). Run inside WSL/Linux with Docker. Writes
# the connection URL (valkey://rusty:<password>@127.0.0.1:<port>) to $HOME/.rusty-kv-test-url.<run id> (mode
# 0600) and prints only a success line. Idempotent per run id (RUSTY_KV_RUN_ID, default "dev"): if that run's
# container is already running it is reused. Other runs' containers are never touched.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
umask 077
# shellcheck source=/dev/null
. scripts/kv-lib.sh
IMAGE_REF="$(awk '$1 == "valkey/valkey:9-alpine" { print $2 }' deploy/images.lock)"

cleanup() {
  RUSTY_KV_PASSWORD=unused "${KV_COMPOSE[@]}" down -v >/dev/null 2>&1 || true
  docker rm -f "$KV_NAME" >/dev/null 2>&1 || true
  rm -f "$KV_URL_FILE"
}

verify_server() {
  local version image
  version="$(docker exec "$KV_NAME" valkey-cli --user rusty info server 2>/dev/null | awk -F: '$1 == "valkey_version" { print $2 }' | tr -d '\r' || true)"
  image="$(docker inspect -f '{{.Config.Image}}' "$KV_NAME" 2>/dev/null || true)"
  [[ "$version" =~ ^9\.[0-9]+\.[0-9]+$ ]] && [ "$image" = "$IMAGE_REF" ]
}

if [ "$(docker inspect -f '{{.State.Running}}' "$KV_NAME" 2>/dev/null || true)" = "true" ] && [ -s "$KV_URL_FILE" ] && verify_server; then
  echo "kv-up: already running; URL in $KV_URL_FILE"
  exit 0
fi

# Stale container or stale URL file: start clean.
cleanup

PASSWORD="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
[ "${#PASSWORD}" -eq 48 ] || { echo "kv-up: FAILED to generate a password" >&2; exit 1; }

export RUSTY_KV_PASSWORD="$PASSWORD"
if ! "${KV_COMPOSE[@]}" up -d --wait --wait-timeout 60 >/dev/null 2>&1; then
  echo "kv-up: FAILED to start (inspect: docker logs $KV_NAME)" >&2
  cleanup
  exit 1
fi
if ! verify_server; then
  echo "kv-up: FAILED image/version verification" >&2
  cleanup
  exit 1
fi
MAPPING="$("${KV_COMPOSE[@]}" port valkey 6379 2>/dev/null || true)"
PORT="${MAPPING##*:}"
if ! [[ "$PORT" =~ ^[0-9]+$ ]]; then
  echo "kv-up: FAILED to discover the loopback port" >&2
  cleanup
  exit 1
fi
# The test container is loopback-only and has no TLS.
printf 'valkey://rusty:%s@127.0.0.1:%s\n' "$PASSWORD" "$PORT" > "$KV_URL_FILE"
chmod 600 "$KV_URL_FILE"
echo "kv-up: Valkey ready on 127.0.0.1:$PORT; URL in $KV_URL_FILE"
