#!/usr/bin/env bash
# Remove this run's test Valkey container, its tmpfs data and its URL file. Idempotent.
# Only touches the run named by RUSTY_KV_RUN_ID (default "dev"); other runs are left alone, and a container is
# only removed when it carries this run's ownership label.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
# shellcheck source=/dev/null
. scripts/kv-lib.sh
RUSTY_KV_PASSWORD=unused "${KV_COMPOSE[@]}" down -v >/dev/null 2>&1 || true
owner="$(docker inspect -f '{{index .Config.Labels "rusty.test-kv-run"}}' "$KV_NAME" 2>/dev/null || true)"
if [ -n "$owner" ] && [ "$owner" = "$KV_RUN_ID" ]; then
  docker rm -f "$KV_NAME" >/dev/null 2>&1 || true
fi
rm -f "$KV_URL_FILE"
echo "kv-down: done ($KV_NAME)"
exit 0
