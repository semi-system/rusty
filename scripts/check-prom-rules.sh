#!/usr/bin/env bash
# Gate: the sample Prometheus configuration and alert rules of deploy/monitoring pass `promtool` (run in the Prometheus image pinned in deploy/images.lock). Needs Docker.
# Usage: bash scripts/check-prom-rules.sh [ROOT]
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
image="$(awk '$1 == "prom/prometheus:v3.9.1" { print $2 }' "$ROOT/deploy/images.lock")"
[ -n "$image" ] || { echo "prom/prometheus is not pinned in deploy/images.lock" >&2; exit 1; }
# The configuration names its rule file by the path inside the container, so both files are mounted there.
out="$(docker run --rm --entrypoint promtool \
  -v "$ROOT/deploy/monitoring/prometheus.yml:/etc/prometheus/prometheus.yml:ro" \
  -v "$ROOT/deploy/monitoring/rusty.rules.yml:/etc/prometheus/rusty.rules.yml:ro" \
  "$image" check config /etc/prometheus/prometheus.yml 2>&1)" || { echo "$out" >&2; echo "FAIL: the Prometheus configuration or its rules are not valid" >&2; exit 1; }
echo "$out" | tail -4
echo "prometheus rules OK"
