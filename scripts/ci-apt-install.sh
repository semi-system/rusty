#!/usr/bin/env bash
# Install apt packages on a CI runner, tolerating a flaky mirror: short network timeouts and a
# few attempts, instead of one 3-minute stall and a red build.
set -euo pipefail
[ "$#" -gt 0 ] || { echo "usage: $0 package..." >&2; exit 2; }
opts=(-o Acquire::Retries=3 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20)
for attempt in 1 2 3; do
  if sudo apt-get "${opts[@]}" update && sudo apt-get "${opts[@]}" install -y --no-install-recommends "$@"; then
    exit 0
  fi
  echo "apt attempt $attempt failed" >&2
  sleep $((attempt * 10))
done
echo "error: could not install: $*" >&2
exit 1
