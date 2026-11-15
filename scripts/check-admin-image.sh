#!/usr/bin/env bash
# Property checks of the built admin web image (`docker build --target admin`). Usage: check-admin-image.sh IMAGE
#   - the image runs as a numeric non-root user, has no shell, exposes only 8080, carries its OCI labels;
#   - the binary is statically linked and has the front end embedded (the index page is inside it);
#   - `--help` runs; a start without a configuration file exits 2 (fail closed) on a read-only root filesystem
#     with every capability dropped, and says nothing on standard output (the setup token is the only thing that may go there).
# Needs Docker on the host.
set -euo pipefail
IMAGE="${1:?usage: check-admin-image.sh IMAGE}"
NAME="rusty-admin-check-$$"
TMPD="$(mktemp -d)"
cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$TMPD"
}
trap cleanup EXIT
ok() { echo "ok: $*"; }
die() { echo "FAIL: $*" >&2; exit 1; }

user="$(docker image inspect "$IMAGE" --format '{{.Config.User}}')"
[[ "$user" =~ ^[1-9][0-9]*(:[0-9]+)?$ ]] || die "image user '$user' is not a numeric non-root user"
ok "runs as numeric user $user"

if docker run --rm --entrypoint /bin/sh "$IMAGE" -c true >/dev/null 2>&1; then die "the image contains a shell"; fi
ok "no shell"

ports="$(docker image inspect "$IMAGE" --format '{{json .Config.ExposedPorts}}')"
[ "$ports" = '{"8080/tcp":{}}' ] || die "exposed ports are not just 8080: $ports"
ok "exposes 8080 only"

entry="$(docker image inspect "$IMAGE" --format '{{json .Config.Entrypoint}}')"
[ "$entry" = '["/usr/local/bin/rusty-admin"]' ] || die "unexpected entrypoint: $entry"
hc="$(docker image inspect "$IMAGE" --format '{{json .Config.Healthcheck.Test}}')"
[[ "$hc" == *'"/usr/local/bin/rusty-admin"'*'"healthcheck"'* ]] || die "HEALTHCHECK is not 'rusty-admin healthcheck': $hc"
ok "HEALTHCHECK $hc"
for l in source revision version created; do
  docker image inspect "$IMAGE" --format "{{index .Config.Labels \"org.opencontainers.image.$l\"}}" >/dev/null || die "label $l"
done
ok "entrypoint and OCI labels"

docker create --name "$NAME" "$IMAGE" >/dev/null
docker cp "$NAME:/usr/local/bin/rusty-admin" "$TMPD/rusty-admin"
docker rm -f "$NAME" >/dev/null
if command -v file >/dev/null 2>&1; then
  file -L "$TMPD/rusty-admin" | grep -qE "statically linked|static-pie linked" || die "binary is not static: $(file -L "$TMPD/rusty-admin")"
  ok "static binary"
fi
grep -qa '<div id="app">' "$TMPD/rusty-admin" || die "the front end is not embedded in the binary (no index page inside it)"
ok "front end embedded"

docker run --rm "$IMAGE" --help >/dev/null 2>&1 || die "--help did not exit 0"
ok "--help"

rc=0
out="$(docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges:true "$IMAGE" 2>"$TMPD/err")" || rc=$?
[ "$rc" = 2 ] || die "a start without a configuration exited $rc, not 2"
[ -z "$out" ] || die "a refused start wrote to standard output"
ok "no configuration: exit 2, nothing on standard output"

size="$(docker image inspect "$IMAGE" --format '{{.Size}}')"
echo "size: $((size / 1024 / 1024)) MiB"
