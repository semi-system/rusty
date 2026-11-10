#!/usr/bin/env bash
# Sync this worktree (including uncommitted changes) to the build server:
#   bash scripts/remote-sync.sh             sync to /srv/rusty/work/<worktree>/
#   bash scripts/remote-sync.sh --pin-host  record the server SSH host key (first use; verify the fingerprint!)
#   bash scripts/remote-sync.sh --check     show the effective configuration and test the connection
set -uo pipefail
# shellcheck source=/dev/null
. "$(dirname "${BASH_SOURCE[0]}")/remote-lib.sh"
rl_load_env
case "${1:-}" in
  "")
    rl_require_config; rl_check_known_hosts; rl_sync; exit $? ;;
  --pin-host)
    rl_require_config
    command -v ssh-keyscan >/dev/null 2>&1 || rl_die "ssh-keyscan not found"
    tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT
    ssh-keyscan -T 10 -p "$BUILD_PORT" -t ed25519 "$BUILD_HOST" > "$tmp" 2>/dev/null
    [ -s "$tmp" ] || rl_die "could not fetch the host key from $BUILD_HOST:$BUILD_PORT"
    echo "Host key fingerprint offered by $BUILD_HOST (compare with the one printed by setup-ubuntu.sh on the VM):"
    ssh-keygen -lf "$tmp"
    if [ -s "$BUILD_KNOWN_HOSTS" ] && ! cmp -s "$tmp" "$BUILD_KNOWN_HOSTS"; then
      rl_die "$BUILD_KNOWN_HOSTS already pins a DIFFERENT key; if the VM was rebuilt, delete that file and re-run"
    fi
    cp "$tmp" "$BUILD_KNOWN_HOSTS"
    echo "pinned in $BUILD_KNOWN_HOSTS" ;;
  --check)
    rl_require_config
    echo "host=$BUILD_HOST user=$BUILD_USER port=$BUILD_PORT key=$BUILD_KEY known_hosts=$BUILD_KNOWN_HOSTS sync=$(rl_pick_sync)"
    echo "worktree=$(rl_worktree_name) remote dir=$(rl_work_dir "$(rl_worktree_name)")"
    rl_check_known_hosts
    rl_ssh 'echo "connected: $(id -un)@$(hostname)"; ls -d /srv/rusty/work /srv/rusty/target /srv/rusty/logs' ;;
  *)
    echo "usage: remote-sync.sh [--pin-host|--check]" >&2; exit 2 ;;
esac
