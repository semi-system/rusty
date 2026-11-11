#!/usr/bin/env bash
# One command for the full CI gate: the remote build server when .rusty-remote.env exists,
# else WSL. RUSTY_BUILD=remote|wsl forces one. Extra arguments go to the chosen script
# (remote-ci.sh: --detach, --status, --tail, --wait; wsl-ci.sh takes none).
set -uo pipefail
here="$(dirname "${BASH_SOURCE[0]}")"
root="$(cd "$here/.." && pwd)"
pick="${RUSTY_BUILD:-}"
if [ -z "$pick" ]; then
  if [ -f "${RUSTY_REMOTE_ENV:-$root/.rusty-remote.env}" ]; then pick=remote; else pick=wsl; fi
fi
case "$pick" in
  remote) exec bash "$here/remote-ci.sh" "$@" ;;
  wsl) exec bash "$here/wsl-ci.sh" "$@" ;;
  *) echo "ci-run: RUSTY_BUILD must be remote or wsl" >&2; exit 2 ;;
esac
