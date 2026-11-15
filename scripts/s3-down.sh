#!/usr/bin/env bash
# Stop the throw-away test S3 server of this run id (default "dev") and remove its settings file. Other runs are never touched.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
# shellcheck source=/dev/null
. scripts/s3-lib.sh
label="$(docker inspect -f '{{index .Config.Labels "rusty.test-s3-run"}}' "$S3_NAME" 2>/dev/null || true)"
if [ "$label" = "$S3_RUN_ID" ]; then
  RUSTY_S3_KEY=unused RUSTY_S3_SECRET=unused "${S3_COMPOSE[@]}" down -v >/dev/null 2>&1 || true
  docker rm -f "$S3_NAME" >/dev/null 2>&1 || true
fi
rm -f "$S3_ENV_FILE"
echo "s3-down: done"
