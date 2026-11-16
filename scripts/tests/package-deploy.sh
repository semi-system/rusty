#!/usr/bin/env bash
# The deployment kit: what it holds, what it leaves out, that it is the same bytes twice, and that the launcher reaches the tool.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
bash scripts/package-deploy.sh test1 "$tmp/a" >/dev/null
bash scripts/package-deploy.sh test1 "$tmp/b" >/dev/null
cmp "$tmp/a/rusty-deploy-test1.tar.gz" "$tmp/b/rusty-deploy-test1.tar.gz" || { echo "package-deploy: the archive is not reproducible" >&2; exit 1; }
( cd "$tmp/a" && sha256sum -c rusty-deploy-test1.tar.gz.sha256 >/dev/null ) || { echo "package-deploy: the checksum file does not match" >&2; exit 1; }
list="$(tar -tzf "$tmp/a/rusty-deploy-test1.tar.gz")"
for want in rusty-deploy-test1/rusty-swarm rusty-deploy-test1/README.txt rusty-deploy-test1/swarm.env.example rusty-deploy-test1/deploy/swarm/rusty-swarm rusty-deploy-test1/deploy/db/roles.sql rusty-deploy-test1/deploy/images.lock; do
  printf '%s\n' "$list" | grep -qx "$want" || { echo "package-deploy: $want is not in the kit" >&2; exit 1; }
done
for never in local-cluster.sh local.env deploy/dev deploy/buildserver; do
  ! printf '%s\n' "$list" | grep -q "$never" || { echo "package-deploy: $never must not be in the kit" >&2; exit 1; }
done
tar -xzf "$tmp/a/rusty-deploy-test1.tar.gz" -C "$tmp"
# Unpacked, the launcher finds the tool (no inventory yet: it says so, and prints where it looked).
out="$("$tmp/rusty-deploy-test1/rusty-swarm" cluster check 2>&1 || true)"
case "$out" in *"no inventory at $tmp/rusty-deploy-test1/swarm.env"*) ;; *) echo "package-deploy: the launcher did not reach the tool: $out" >&2; exit 1 ;; esac
[ -x "$tmp/rusty-deploy-test1/deploy/swarm/rusty-swarm" ] || { echo "package-deploy: the tool is not executable in the kit" >&2; exit 1; }
echo "package-deploy OK"
