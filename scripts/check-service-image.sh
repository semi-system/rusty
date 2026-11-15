#!/usr/bin/env bash
# Property checks of a built service image (`docker build --target rusty-<service>`). Usage: check-service-image.sh SERVICE IMAGE
# SERVICE is smtp-in, submission, imap, outbound or gc. It checks that the image
#   - runs as a numeric non-root user, has no shell, carries its OCI labels;
#   - exposes exactly the ports of the service (none for outbound and gc);
#   - has the entrypoint and HEALTHCHECK of its own binary, and holds that one executable and no other;
#   - has a statically linked binary;
#   - fails closed: a start without its configuration file exits 1 or 2, on a read-only root filesystem with every capability dropped, and prints nothing to standard output.
# Needs Docker on the host.
set -euo pipefail
SERVICE="${1:?usage: check-service-image.sh SERVICE IMAGE}"
IMAGE="${2:?usage: check-service-image.sh SERVICE IMAGE}"
case "$SERVICE" in
  smtp-in) PORTS='{"25/tcp":{}}' ;;
  submission) PORTS='{"465/tcp":{},"587/tcp":{}}' ;;
  imap) PORTS='{"143/tcp":{},"993/tcp":{}}' ;;
  outbound | gc) PORTS='null' ;;
  *) echo "unknown service: $SERVICE" >&2; exit 2 ;;
esac
BIN="rusty-$SERVICE"
NAME="rusty-$SERVICE-check-$$"
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
[ "$ports" = "$PORTS" ] || die "exposed ports are $ports, expected $PORTS"
ok "exposed ports $ports"

entry="$(docker image inspect "$IMAGE" --format '{{json .Config.Entrypoint}}')"
[ "$entry" = "[\"/usr/local/bin/$BIN\"]" ] || die "unexpected entrypoint: $entry"
hc="$(docker image inspect "$IMAGE" --format '{{json .Config.Healthcheck.Test}}')"
[[ "$hc" == *"\"/usr/local/bin/$BIN\""*'"healthcheck"'* ]] || die "HEALTHCHECK is not '$BIN healthcheck': $hc"
ok "entrypoint and HEALTHCHECK of $BIN"
for l in source revision version created; do
  docker image inspect "$IMAGE" --format "{{index .Config.Labels \"org.opencontainers.image.$l\"}}" >/dev/null || die "label $l"
done
ok "OCI labels"

docker create --name "$NAME" "$IMAGE" >/dev/null
docker export "$NAME" | tar -t 2>/dev/null | grep -E '(^|/)usr/local/bin/[^/]+$' | sed 's#^\./##' | sort > "$TMPD/bins"
[ "$(cat "$TMPD/bins")" = "usr/local/bin/$BIN" ] || die "the image holds executables other than $BIN: $(tr '\n' ' ' < "$TMPD/bins")"
ok "exactly one executable: $BIN"
docker cp "$NAME:/usr/local/bin/$BIN" "$TMPD/$BIN"
if command -v file >/dev/null 2>&1; then
  file -L "$TMPD/$BIN" | grep -qE "statically linked|static-pie linked" || die "binary is not static: $(file -L "$TMPD/$BIN")"
  ok "static binary"
fi

set +e
out="$(docker run --rm --read-only --cap-drop ALL "$IMAGE" 2>"$TMPD/stderr")"
code=$?
set -e
{ [ "$code" -eq 1 ] || [ "$code" -eq 2 ]; } || die "a start without a configuration exits $code (expected 1 or 2)"
[ -z "$out" ] || die "a refused start printed on standard output"
ok "fails closed without a configuration (exit $code, nothing on standard output)"
echo "all checks passed for $IMAGE"
