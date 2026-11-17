#!/usr/bin/env bash
# The deployment kit as ONE file to download: rusty-deploy-VERSION.tar.gz (deploy/ without the development and build-server parts, a launcher and the inventory example at the top).
#
#   scripts/package-deploy.sh VERSION [OUTDIR [PINS]]        (OUTDIR defaults to ./dist)
#
# PINS is a file of image pins (scripts/pin-release-images.sh): its lines are appended to the kit's images.lock, so the kit names the exact images (by digest) built and signed for its release.
# Without PINS the kit still has the pinned third-party images, and the Rusty images are by tag (a development kit).
#
# The operator downloads the file of a release, checks it against SHA256SUMS (signed with cosign, see docs/docker.md), unpacks it and runs `./rusty-swarm init`; nothing else is needed from the repository.
# The archive is reproducible: the same tree and VERSION give the same bytes (sorted names, a fixed time, no owner).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
version="${1:?usage: package-deploy.sh VERSION [OUTDIR]}"
out="${2:-dist}"
pins="${3:-}"
[ -z "$pins" ] || [ -s "$pins" ] || { echo "package-deploy: PINS $pins is missing or empty" >&2; exit 1; }
[[ "$version" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "package-deploy: VERSION may hold letters, digits, . _ - only" >&2; exit 2; }
name="rusty-deploy-$version"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
root="$stage/$name"
mkdir -p "$root/deploy"

# What the kit holds: the swarm tool and its inventory example, the database roles, the pinned images, the standalone stack and the monitoring samples.
for p in deploy/swarm deploy/db deploy/standalone deploy/monitoring deploy/images.lock; do
  [ -e "$p" ] || { echo "package-deploy: $p is missing" >&2; exit 1; }
  mkdir -p "$root/$(dirname "$p")"
  cp -R "$p" "$root/$p"
done
# The throw-away local test cluster and its inventory are for developers, not for a server.
rm -f "$root/deploy/swarm/local-cluster.sh" "$root/deploy/swarm/local.env"
# The release the kit belongs to; with PINS, the digests of its images follow the pins of the third-party images in images.lock.
echo "$version" > "$root/deploy/RELEASE"
if [ -n "$pins" ]; then
  { echo; echo "# Images of release $version (digests of the images built and signed for it):"; cat "$pins"; } >> "$root/deploy/images.lock"
fi

cat > "$root/rusty-swarm" <<'LAUNCHER'
#!/usr/bin/env bash
# The one command of the kit: ./rusty-swarm init  (see ./rusty-swarm for the rest). The inventory is swarm.env next to this file.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export RUSTY_SWARM_ENV="${RUSTY_SWARM_ENV:-$here/swarm.env}"
exec "$here/deploy/swarm/rusty-swarm" "$@"
LAUNCHER
cp "$root/deploy/swarm/swarm.env.example" "$root/swarm.env.example"
cat > "$root/README.txt" <<TXT
Rusty Mail Server - deployment kit $version

  1. cp swarm.env.example swarm.env     and fill in the nodes and the database (the file is yours: keep it out of version control)
  2. ./rusty-swarm init                 Docker is installed where it is missing, the swarm is made, the database is prepared

Check this file against SHA256SUMS of the release before you unpack it (the sums are signed: docs/docker.md).
TXT
chmod 0755 "$root/rusty-swarm" "$root/deploy/swarm/rusty-swarm"
find "$root/deploy" -name '*.sh' -exec chmod 0755 {} +

mkdir -p "$out"
archive="$out/$name.tar.gz"
# Reproducible: names sorted, one fixed time, no owner; gzip without a name or time.
( cd "$stage" && find "$name" -print0 | LC_ALL=C sort -z | tar --null --no-recursion --owner=0 --group=0 --numeric-owner --mtime='2000-01-01 00:00:00 UTC' -cf - -T - ) | gzip -n -9 > "$archive"
( cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256" )
# The same kit as a .zip (for a machine that has no tar, or a person who prefers it), when `zip` is here; times fixed so that it is reproducible too.
if command -v zip >/dev/null 2>&1; then
  find "$stage/$name" -exec touch -d '2000-01-01 00:00:00 UTC' {} +
  outabs="$(cd "$out" && pwd)"
  ( cd "$stage" && find "$name" -print | LC_ALL=C sort | zip -X -q "$outabs/$name.zip" -@ ) || { echo "package-deploy: zip failed" >&2; exit 1; }
fi
echo "package-deploy: $archive ($(wc -c < "$archive") bytes)"
