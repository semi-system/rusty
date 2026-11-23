#!/usr/bin/env bash
# Property checks of the built webmail image (`docker build --target webmail`). Usage: check-webmail-image.sh IMAGE
#   - the image runs as a numeric non-root user, has no shell, exposes only 8090, carries its OCI labels;
#   - the binary is statically linked;
#   - `--help` runs; a start without a configuration file exits 2 (fail closed) on a read-only root filesystem
#     with every capability dropped,.
# Needs Docker on the host.
set -euo pipefail
IMAGE="${1:?usage: check-webmail-image.sh IMAGE}"
NAME="rusty-webmail-check-$$"
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
[ "$ports" = '{"8090/tcp":{}}' ] || die "exposed ports are not just 8090: $ports"
ok "exposes 8090 only"

entry="$(docker image inspect "$IMAGE" --format '{{json .Config.Entrypoint}}')"
[ "$entry" = '["/usr/local/bin/rusty-webmail"]' ] || die "unexpected entrypoint: $entry"
hc="$(docker image inspect "$IMAGE" --format '{{json .Config.Healthcheck.Test}}')"
[[ "$hc" == *'"/usr/local/bin/rusty-webmail"'*'"healthcheck"'* ]] || die "HEALTHCHECK is not 'rusty-webmail healthcheck': $hc"
ok "HEALTHCHECK $hc"
for l in source revision version created; do
  docker image inspect "$IMAGE" --format "{{index .Config.Labels \"org.opencontainers.image.$l\"}}" >/dev/null || die "label $l"
done
ok "entrypoint and OCI labels"

docker create --name "$NAME" "$IMAGE" >/dev/null
docker cp "$NAME:/usr/local/bin/rusty-webmail" "$TMPD/rusty-webmail"
docker rm -f "$NAME" >/dev/null
if command -v file >/dev/null 2>&1; then
  file -L "$TMPD/rusty-webmail" | grep -qE "statically linked|static-pie linked" || die "binary is not static: $(file -L "$TMPD/rusty-webmail")"
  ok "static binary"
fi

docker run --rm "$IMAGE" --help >/dev/null 2>&1 || die "--help did not exit 0"
ok "--help"

rc=0
out="$(docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges:true "$IMAGE" 2>"$TMPD/err")" || rc=$?
[ "$rc" = 2 ] || die "a start without a configuration exited $rc, not 2"
[ -z "$out" ] || die "a refused start wrote to standard output"
ok "no configuration: exit 2, nothing on standard output"

size="$(docker image inspect "$IMAGE" --format '{{.Size}}')"
echo "size: $((size / 1024 / 1024)) MiB"
