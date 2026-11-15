#!/usr/bin/env bash
# Start the throw-away test S3 server (RustFS, deploy/dev/s3.compose.yaml). Run inside WSL/Linux with Docker. Writes the settings the S3 tests read
# (RUSTY_TEST_S3_ENDPOINT, RUSTY_TEST_S3_KEY, RUSTY_TEST_S3_SECRET as `NAME=value` lines) to $HOME/.rusty-s3-test-env.<run id> (mode 0600) and prints only a success
# line. Idempotent per run id (RUSTY_S3_RUN_ID, default "dev"): if that run's container is already running it is reused. Other runs' containers are never touched.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
umask 077
# shellcheck source=/dev/null
. scripts/s3-lib.sh
IMAGE_REF="$(awk '$1 == "rustfs/rustfs:latest" { print $2 }' deploy/images.lock)"
[ -n "$IMAGE_REF" ] || { echo "s3-up: rustfs/rustfs is not pinned in deploy/images.lock" >&2; exit 1; }

cleanup() {
  RUSTY_S3_KEY=unused RUSTY_S3_SECRET=unused "${S3_COMPOSE[@]}" down -v >/dev/null 2>&1 || true
  docker rm -f "$S3_NAME" >/dev/null 2>&1 || true
  rm -f "$S3_ENV_FILE"
}

# Any HTTP answer (a 403 without credentials is the normal one) means the server is up.
wait_http() {
  local port="$1" code
  for _ in $(seq 1 60); do
    code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/" 2>/dev/null || true)"
    case "$code" in 2??|3??|4??) return 0 ;; esac
    sleep 1
  done
  return 1
}

port_of() {
  local mapping
  mapping="$("${S3_COMPOSE[@]}" port rustfs 9000 2>/dev/null || true)"
  printf '%s' "${mapping##*:}"
}

if [ "$(docker inspect -f '{{.State.Running}}' "$S3_NAME" 2>/dev/null || true)" = "true" ] && [ -s "$S3_ENV_FILE" ] \
  && [ "$(docker inspect -f '{{.Config.Image}}' "$S3_NAME" 2>/dev/null || true)" = "$IMAGE_REF" ]; then
  echo "s3-up: already running; settings in $S3_ENV_FILE"
  exit 0
fi

# Stale container or stale settings file: start clean.
cleanup

KEY="$(od -An -N10 -tx1 /dev/urandom | tr -d ' \n')"
SECRET="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
{ [ "${#KEY}" -eq 20 ] && [ "${#SECRET}" -eq 48 ]; } || { echo "s3-up: FAILED to generate a key pair" >&2; exit 1; }

export RUSTY_S3_KEY="$KEY" RUSTY_S3_SECRET="$SECRET"
if ! "${S3_COMPOSE[@]}" up -d >/dev/null 2>&1; then
  echo "s3-up: FAILED to start (inspect: docker logs $S3_NAME)" >&2
  cleanup
  exit 1
fi
PORT="$(port_of)"
if ! [[ "$PORT" =~ ^[0-9]+$ ]]; then
  echo "s3-up: FAILED to discover the loopback port" >&2
  cleanup
  exit 1
fi
if ! wait_http "$PORT"; then
  echo "s3-up: FAILED, the server did not answer (inspect: docker logs $S3_NAME)" >&2
  cleanup
  exit 1
fi
# The test container is loopback-only and has no TLS.
printf 'RUSTY_TEST_S3_ENDPOINT=http://127.0.0.1:%s\nRUSTY_TEST_S3_KEY=%s\nRUSTY_TEST_S3_SECRET=%s\n' "$PORT" "$KEY" "$SECRET" > "$S3_ENV_FILE"
chmod 600 "$S3_ENV_FILE"
echo "s3-up: RustFS ready on 127.0.0.1:$PORT; settings in $S3_ENV_FILE"
