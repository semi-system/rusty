#!/usr/bin/env bash
# Property checks of a built Rusty image. Usage: check-image.sh IMAGE [dev]
#   - `rusty --version` runs; the image runs as a numeric non-root user; there is no shell;
#   - the binary is statically linked; a HEALTHCHECK is configured; OCI labels are present;
#   - a production start without a config exits 1 (fail closed) on a read-only root filesystem;
#   - with `dev` (an image built with --build-arg DEV_CERT=1): next to a throw-away Postgres (image pinned in
#     deploy/images.lock, on a private Docker network) the schema is built with `rusty ctl --dev db migrate`, then
#     the daemon runs with --read-only, all capabilities dropped, and `rusty healthcheck` inside the container
#     reports healthy.
# Needs Docker on the host; prints the image size at the end.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${1:?usage: check-image.sh IMAGE [dev]}"
MODE="${2:-prod}"
NAME="rusty-check-$$"
TMPD="$(mktemp -d)"
cleanup() {
  docker rm -f "$NAME" "$NAME-pg" >/dev/null 2>&1 || true
  docker volume rm -f "$NAME-data" >/dev/null 2>&1 || true
  docker network rm "$NAME-net" >/dev/null 2>&1 || true
  rm -rf "$TMPD"
}
trap cleanup EXIT
ok() { echo "ok: $*"; }
die() { echo "FAIL: $*" >&2; exit 1; }

v="$(docker run --rm "$IMAGE" --version)"
[[ "$v" =~ ^rusty\ [0-9]+\.[0-9]+\.[0-9]+ ]] || die "unexpected --version output: $v"
ok "$v"

user="$(docker image inspect "$IMAGE" --format '{{.Config.User}}')"
[[ "$user" =~ ^[1-9][0-9]*(:[0-9]+)?$ ]] || die "image user '$user' is not a numeric non-root user"
ok "runs as numeric user $user"

if docker run --rm --entrypoint /bin/sh "$IMAGE" -c true >/dev/null 2>&1; then die "the image contains a shell"; fi
if docker run --rm --entrypoint /busybox/sh "$IMAGE" -c true >/dev/null 2>&1; then die "the image contains a shell"; fi
ok "no shell (/bin/sh does not exist)"

hc="$(docker image inspect "$IMAGE" --format '{{json .Config.Healthcheck.Test}}')"
[[ "$hc" == *'"/usr/local/bin/rusty"'*'"healthcheck"'* ]] || die "HEALTHCHECK is not 'rusty healthcheck': $hc"
ok "HEALTHCHECK $hc"

for l in source revision version created; do
  docker image inspect "$IMAGE" --format "{{index .Config.Labels \"org.opencontainers.image.$l\"}}" >/dev/null || die "label $l"
done
ok "OCI labels present: $(docker image inspect "$IMAGE" --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')"

docker create --name "$NAME" "$IMAGE" >/dev/null
docker cp "$NAME:/usr/local/bin/rusty" "$TMPD/rusty"
docker rm -f "$NAME" >/dev/null
if command -v file >/dev/null 2>&1; then
  file -L "$TMPD/rusty" | grep -qE "statically linked|static-pie linked" || die "binary is not static: $(file -L "$TMPD/rusty")"
  ok "static: $(file -L "$TMPD/rusty" | cut -d, -f1-4)"
fi
if command -v ldd >/dev/null 2>&1; then
  if ldd "$TMPD/rusty" >/dev/null 2>&1; then die "ldd found dynamic dependencies"; fi
  ok "ldd: not a dynamic executable"
fi

rc=0
docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges "$IMAGE" >/dev/null 2>&1 || rc=$?
[ "$rc" = 1 ] || die "production start without a config exited $rc, expected 1"
ok "production start without a config fails closed (exit 1) on a read-only root"

if [ "$MODE" = dev ]; then
  PGIMG="$(awk '$1 == "postgres:18" { print $2 }' "$ROOT/deploy/images.lock")"
  [ -n "$PGIMG" ] || die "no postgres image in deploy/images.lock"
  PGPW="$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
  docker network create "$NAME-net" >/dev/null
  docker run -d --name "$NAME-pg" --network "$NAME-net" --network-alias pg -e POSTGRES_USER=rusty -e POSTGRES_DB=rusty \
    -e POSTGRES_PASSWORD="$PGPW" "$PGIMG" -c fsync=off >/dev/null
  pgup=0
  for _ in $(seq 1 60); do
    if docker exec "$NAME-pg" pg_isready -h 127.0.0.1 -U rusty -d rusty >/dev/null 2>&1; then pgup=1; break; fi
    sleep 1
  done
  [ "$pgup" = 1 ] || { docker logs "$NAME-pg" >&2; die "the test Postgres never became ready"; }
  sleep 2
  ok "throw-away Postgres is up"
  # The dev profile names its database in the environment; the private link has no TLS (explicit, warned at start).
  DEVENV=(-e RUSTY_DEV_PG_URL=postgres://rusty@pg:5432/rusty -e RUSTY_DEV_PG_PASSWORD="$PGPW" -e RUSTY_DEV_PG_SSLMODE=disable)
  docker volume create "$NAME-data" >/dev/null
  docker run --rm --network "$NAME-net" --read-only --cap-drop ALL --security-opt no-new-privileges \
    -v "$NAME-data:/var/lib/rusty" "${DEVENV[@]}" "$IMAGE" ctl --dev --data-dir /var/lib/rusty/dev db migrate --without-roles >/dev/null \
    || die "rusty ctl db migrate failed"
  ok "schema built with rusty ctl --dev db migrate"
  docker run -d --name "$NAME" --network "$NAME-net" --read-only --cap-drop ALL --security-opt no-new-privileges \
    -v "$NAME-data:/var/lib/rusty" "${DEVENV[@]}" "$IMAGE" d --dev --data-dir /var/lib/rusty/dev --bind 0.0.0.0 >/dev/null
  up=0
  for _ in $(seq 1 60); do
    if docker exec "$NAME" /usr/local/bin/rusty healthcheck --dev --data-dir /var/lib/rusty/dev >/dev/null 2>&1; then up=1; break; fi
    [ "$(docker inspect -f '{{.State.Running}}' "$NAME")" = true ] || { docker logs "$NAME" >&2; die "daemon exited"; }
    sleep 1
  done
  [ "$up" = 1 ] || { docker logs "$NAME" >&2; die "healthcheck never succeeded"; }
  ok "daemon on a read-only root, caps dropped: $(docker exec "$NAME" /usr/local/bin/rusty healthcheck --dev --data-dir /var/lib/rusty/dev)"
  docker stop -t 30 "$NAME" >/dev/null
  [ "$(docker inspect -f '{{.State.ExitCode}}' "$NAME")" = 0 ] || die "SIGTERM did not end in exit code 0"
  ok "SIGTERM stops the daemon with exit code 0"
fi

echo "size: $(docker image inspect "$IMAGE" --format '{{.Size}}') bytes"
echo "IMAGE CHECKS PASSED"
