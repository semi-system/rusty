#!/usr/bin/env bash
# Install apt packages on a CI runner, tolerating a flaky mirror: the runner image points apt at
# an Azure mirror list (azure.archive.ubuntu.com) that sometimes does not resolve, so the sources
# are pointed at the canonical archive first; short network timeouts and a few attempts follow.
set -euo pipefail
[ "$#" -gt 0 ] || { echo "usage: $0 package..." >&2; exit 2; }

# Ubuntu 24.04 runners: deb822 sources with "URIs: mirror+file:/etc/apt/apt-mirrors.txt".
for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.sources /etc/apt/sources.list.d/*.list; do
  [ -f "$f" ] || continue
  if grep -q 'apt-mirrors.txt' "$f"; then
    sudo sed -i 's#mirror+file:/etc/apt/apt-mirrors.txt#http://archive.ubuntu.com/ubuntu/#' "$f"
  fi
done

opts=(-o Acquire::Retries=3 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20 -o Acquire::ForceIPv4=true)
for attempt in 1 2 3; do
  if sudo apt-get "${opts[@]}" update && sudo apt-get "${opts[@]}" install -y --no-install-recommends "$@"; then
    exit 0
  fi
  echo "apt attempt $attempt failed" >&2
  sleep $((attempt * 10))
done
echo "error: could not install: $*" >&2
exit 1
