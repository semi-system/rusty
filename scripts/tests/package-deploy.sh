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
# The release kit with pins: a stub docker and cosign stand for the registry. The kit names every service image by digest, for exactly this release; an unsigned image is never pinned.
bin="$tmp/bin"; mkdir -p "$bin"
cat > "$bin/docker" <<'STUB'
#!/usr/bin/env bash
# docker buildx imagetools inspect REF --format ...: a digest made from the reference
ref="$4"; printf 'sha256:%s\n' "$(printf '%s' "$ref" | sha256sum | cut -c1-64)"
STUB
cat > "$bin/cosign" <<'STUB'
#!/usr/bin/env bash
[ -z "${COSIGN_FAIL:-}" ]
STUB
chmod +x "$bin/docker" "$bin/cosign"
PATH="$bin:$PATH" bash scripts/pin-release-images.sh v1.2.3 Owner/Rusty "$tmp/pins.txt" >/dev/null
[ "$(wc -l < "$tmp/pins.txt")" = 7 ] || { echo "package-deploy: 7 service images are pinned, got $(wc -l < "$tmp/pins.txt")" >&2; exit 1; }
grep -qE "^ghcr.io/owner/rusty-imap:1.2.3  ghcr.io/owner/rusty-imap@sha256:[0-9a-f]{64}  " "$tmp/pins.txt" || { echo "package-deploy: the imap pin is wrong: $(cat "$tmp/pins.txt")" >&2; exit 1; }
grep -q "^ghcr.io/owner/rusty:1.2.3  ghcr.io/owner/rusty@sha256:" "$tmp/pins.txt" || { echo "package-deploy: the all-in-one image is not pinned" >&2; exit 1; }
bash scripts/package-deploy.sh v1.2.3 "$tmp/k" "$tmp/pins.txt" >/dev/null
tar -xzf "$tmp/k/rusty-deploy-v1.2.3.tar.gz" -C "$tmp/k"
lock="$tmp/k/rusty-deploy-v1.2.3/deploy/images.lock"
[ "$(grep -c "(release v1.2.3)" "$lock")" = 7 ] || { echo "package-deploy: the kit's images.lock lacks the release pins" >&2; exit 1; }
grep -q "^postgres:18 " "$lock" || { echo "package-deploy: the third-party pins are gone from the kit's images.lock" >&2; exit 1; }
[ "$(cat "$tmp/k/rusty-deploy-v1.2.3/deploy/RELEASE")" = v1.2.3 ] || { echo "package-deploy: RELEASE is wrong" >&2; exit 1; }
# An image whose signature does not verify is not pinned (and the script gives up at the deadline); a tag that is not a version is refused.
if PATH="$bin:$PATH" COSIGN_FAIL=1 PIN_TIMEOUT_SECS=1 PIN_INTERVAL_SECS=1 bash scripts/pin-release-images.sh v1.2.3 Owner/Rusty "$tmp/pins2.txt" >/dev/null 2>&1; then echo "package-deploy: an unsigned image was pinned" >&2; exit 1; fi
if bash scripts/pin-release-images.sh main Owner/Rusty "$tmp/pins3.txt" >/dev/null 2>&1; then echo "package-deploy: a tag that is not a version was accepted" >&2; exit 1; fi
echo "package-deploy OK"
