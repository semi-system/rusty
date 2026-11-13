#!/usr/bin/env bash
# SERVER-SIDE runner (runs on the Ubuntu build server, started by scripts/remote-*.sh over SSH).
#   remote-runner.sh <cargo|ci> <worktree-name> <timestamp> [cargo args...]
# Takes the per-worktree lock (and, for ci, the global CI lock), runs the command in
# /srv/rusty/work/<name> with CARGO_TARGET_DIR=/srv/rusty/target/<name>, tees everything to
# /srv/rusty/logs/<name>-<timestamp>-<mode>.log and records the exit code in <log>.rc.
# Exit 75 = a lock is held by someone else.
set -uo pipefail
# CI parity with the GitHub runners and WSL (umask 022). Ubuntu's default 002 makes bare temp dirs group-writable,
# which the blob store (correctly) refuses; tests must not depend on the umask, but keep the runner predictable.
umask 022

mode="${1:-}"; name="${2:-}"; ts="${3:-}"
[ "$#" -ge 3 ] || { echo "usage: remote-runner.sh <cargo|ci> <name> <ts> [args]" >&2; exit 2; }
shift 3
case "$mode" in cargo|ci) ;; *) echo "runner: bad mode '$mode'" >&2; exit 2 ;; esac
[[ "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]] && [[ "$name" != *..* ]] || { echo "runner: bad worktree name" >&2; exit 2; }
[[ "$ts" =~ ^[0-9]{8}-[0-9]{6}$ ]] || { echo "runner: bad timestamp" >&2; exit 2; }

root=/srv/rusty
work="$root/work/$name"; target="$root/target/$name"; logs="$root/logs"
log="$logs/$name-$ts-$mode.log"
[ -d "$work" ] || { echo "runner: $work does not exist (sync first)" >&2; exit 2; }
mkdir -p "$logs" "$target"
wait_s="${BUILD_LOCK_WAIT:-0}"

# Per-worktree lock: two runs of the same worktree must not share a target dir.
exec 9>"$logs/.lock-wt-$name"
if ! flock -w "$wait_s" 9; then
  echo "runner: worktree '$name' is busy: $(cat "$logs/.lock-wt-$name.holder" 2>/dev/null || echo unknown holder)" >&2
  exit 75
fi
echo "$mode $name pid=$$ since=$(date -Is) log=$log" > "$logs/.lock-wt-$name.holder"
if [ "$mode" = ci ]; then
  exec 8>"$logs/.lock-ci-global"
  if ! flock -w "$wait_s" 8; then
    echo "runner: the global CI lock is held by: $(cat "$logs/.lock-ci-global.holder" 2>/dev/null || echo unknown holder)" >&2
    exit 75
  fi
  echo "ci $name pid=$$ since=$(date -Is) log=$log" > "$logs/.lock-ci-global.holder"
fi

echo "$log" > "$logs/latest-$name"
rm -f "$log.rc"
# shellcheck source=/dev/null
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export CARGO_TARGET_DIR="$target"
# Keep the (small) build server disk usable: no incremental dirs, line-table debuginfo only.
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only
if [ "$mode" = cargo ] && [ -z "${RUSTY_PG_RUN_ID:-}" ]; then
  # same charset rule as scripts/pg-lib.sh; one stable id per worktree for manual cargo runs
  RUSTY_PG_RUN_ID="wt$(printf '%s' "$name" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9' x | cut -c1-20)"
  export RUSTY_PG_RUN_ID
fi

{
  echo "== rusty remote run: mode=$mode worktree=$name host=$(hostname) user=$(id -un) start=$(date -Is)"
  echo "== log: $log"
  cd "$work" || exit 127
  if [ "$mode" = ci ]; then bash scripts/ci.sh; else cargo "$@"; fi
} 2>&1 | tee -a "$log"
rc=${PIPESTATUS[0]}
echo "== exit code: $rc ($(date -Is))" | tee -a "$log"
echo "$rc" > "$log.rc"
rm -f "$logs/.lock-wt-$name.holder"
[ "$mode" != ci ] || rm -f "$logs/.lock-ci-global.holder"
exit "$rc"
