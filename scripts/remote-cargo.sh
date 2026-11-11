#!/usr/bin/env bash
# Run cargo on the remote build server for this worktree, from Git Bash on Windows:
#   bash scripts/remote-cargo.sh test --workspace
# Syncs the worktree first (RUSTY_REMOTE_NO_SYNC=1 skips), streams output live, keeps the log on
# the server in /srv/rusty/logs and propagates cargo's exit code. Config: docs/buildserver.md.
set -uo pipefail
# shellcheck source=/dev/null
. "$(dirname "${BASH_SOURCE[0]}")/remote-lib.sh"
rl_load_env
rl_require_config
rl_check_known_hosts
name="$(rl_worktree_name)" || exit 2
[ -n "${RUSTY_REMOTE_NO_SYNC:-}" ] || rl_sync || exit $?
ts="$(rl_timestamp)"
envs=()
while IFS= read -r l; do [ -n "$l" ] && envs+=("$l"); done < <(rl_forward_env)
echo "remote: cargo $* (log: $(rl_logs_dir)/$name-$ts-cargo.log on $BUILD_HOST)" >&2
rl_ssh "env BUILD_LOCK_WAIT=$BUILD_LOCK_WAIT $(rl_quote "${envs[@]}") bash $(rl_quote "$(rl_work_dir "$name")/scripts/remote-runner.sh") cargo $(rl_quote "$name" "$ts" "$@")"
exit $?
