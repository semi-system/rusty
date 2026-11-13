#!/usr/bin/env bash
# Run an arbitrary command in this worktree's directory on the remote build server:
#   bash scripts/remote-run.sh bash scripts/ci-workflows-lint.sh
#   bash scripts/remote-run.sh bash scripts/ci-gates.sh static
# Same rules as remote-cargo.sh: host key pinned, key-only BatchMode ssh, every argument quoted
# exactly, the worktree synced first (RUSTY_REMOTE_NO_SYNC=1 skips), the per-worktree lock taken
# (exit 75 when busy; BUILD_LOCK_WAIT seconds to wait), CARGO_TARGET_DIR set to the worktree's
# target dir, the command's exit code propagated. It does NOT take the global CI lock, so it can
# run next to a detached scripts/ci.sh of another worktree (do not start a second heavy build).
set -uo pipefail
# shellcheck source=/dev/null
. "$(dirname "${BASH_SOURCE[0]}")/remote-lib.sh"
[ "$#" -ge 1 ] || { echo "usage: remote-run.sh <command> [args...]" >&2; exit 2; }
rl_load_env
rl_require_config
rl_check_known_hosts
name="$(rl_worktree_name)" || exit 2
[ -n "${RUSTY_REMOTE_NO_SYNC:-}" ] || rl_sync || exit $?
work="$(rl_work_dir "$name")"; target="$(rl_target_dir "$name")"
rl_safe_remote_path "$work" && rl_safe_remote_path "$target" || rl_die "refusing unsafe remote path"
echo "remote: run $* (on $BUILD_HOST, $work)" >&2
rl_ssh_bash 'set -u; work="$1"; target="$2"; wait_s="$3"; name="$4"; shift 4
  mkdir -p /srv/rusty/logs "$target"
  exec 9>"/srv/rusty/logs/.lock-wt-$name"
  flock -w "$wait_s" 9 || { echo "runner: worktree $name is busy" >&2; exit 75; }
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  export CARGO_TARGET_DIR="$target"
  cd "$work" || exit 127
  "$@"' "$work" "$target" "$BUILD_LOCK_WAIT" "$name" "$@"
exit $?
