#!/usr/bin/env bash
# Run the full CI gate (scripts/ci.sh) on the remote build server.
#   bash scripts/remote-ci.sh              foreground, output streamed live
#   bash scripts/remote-ci.sh --detach     start on the server and return at once (survives this session dying)
#   bash scripts/remote-ci.sh --status     state of the latest run of this worktree
#                                          (exit: the run's rc if finished, 100 running, 101 none, 102 died)
#   bash scripts/remote-ci.sh --tail [N]   last N log lines (default 40);  --follow = tail -F
#   bash scripts/remote-ci.sh --wait       block until the latest run finishes, exit with its exit code
# Only one scripts/ci.sh runs at a time on the server (global flock); the holder is printed when busy.
set -uo pipefail
# shellcheck source=/dev/null
. "$(dirname "${BASH_SOURCE[0]}")/remote-lib.sh"

action=run; tail_n=40; follow=0
while [ $# -gt 0 ]; do
  case "$1" in
    --detach) action=detach ;;
    --status) action="status" ;;
    --tail) action="tail"; if [[ "${2:-}" =~ ^[0-9]+$ ]]; then tail_n="$2"; shift; fi ;;
    --follow) action="tail"; follow=1 ;;
    --wait) action="wait" ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "remote-ci: unknown argument '$1' (see --help)" >&2; exit 2 ;;
  esac
  shift
done

rl_load_env
rl_require_config
rl_check_known_hosts
name="$(rl_worktree_name)" || exit 2
logs="$(rl_logs_dir)"

# Prints "STATE=<running|finished|died|none> RC=<n> LOG=<path>".
remote_state() {
  rl_ssh_bash 'logs="$1"; name="$2"
    latest="$(cat "$logs/latest-$name" 2>/dev/null)"
    [ -n "$latest" ] || { echo "STATE=none RC= LOG="; exit 0; }
    if [ -f "$latest.rc" ]; then echo "STATE=finished RC=$(cat "$latest.rc") LOG=$latest"; exit 0; fi
    if flock -n "$logs/.lock-wt-$name" true 2>/dev/null; then echo "STATE=died RC= LOG=$latest"
    else echo "STATE=running RC= LOG=$latest"; fi' "$logs" "$name"
}
field() { sed -n "s/.*$1=\([^ ]*\).*/\1/p" <<<"$2"; }

case "$action" in
  status)
    st="$(remote_state)" || exit 255
    echo "$st"
    case "$(field STATE "$st")" in
      finished) rc="$(field RC "$st")"; exit "${rc:-1}" ;;
      running) exit 100 ;;
      none) exit 101 ;;
      *) exit 102 ;;
    esac ;;
  tail)
    st="$(remote_state)" || exit 255
    log="$(field LOG "$st")"
    [ -n "$log" ] || { echo "remote-ci: no run recorded for $name" >&2; exit 101; }
    f="-n"; [ "$follow" = 1 ] && f="-F -n"
    # shellcheck disable=SC2016
    rl_ssh_bash 'n="$1"; shift; f="$1"; shift; exec tail $f "$n" "$1"' "$tail_n" "$f" "$log"
    exit $? ;;
  wait)
    deadline=$(( $(date +%s) + ${BUILD_WAIT_TIMEOUT:-7200} ))
    while :; do
      st="$(remote_state)" || exit 255
      case "$(field STATE "$st")" in
        finished) echo "$st"; rc="$(field RC "$st")"; exit "${rc:-1}" ;;
        none) echo "remote-ci: no run recorded for $name" >&2; exit 101 ;;
        died) echo "$st" >&2; echo "remote-ci: the run died without a result" >&2; exit 102 ;;
      esac
      [ "$(date +%s)" -lt "$deadline" ] || { echo "remote-ci: wait timed out" >&2; exit 124; }
      sleep "${BUILD_WAIT_POLL:-10}"
    done ;;
esac

# run / detach: sync, then start the runner.
rl_sync || exit $?
ts="$(rl_timestamp)"
runner="$(rl_work_dir "$name")/scripts/remote-runner.sh"
log="$logs/$name-$ts-ci.log"
envs=()
while IFS= read -r l; do [ -n "$l" ] && envs+=("$l"); done < <(rl_forward_env)
cmd=(env "BUILD_LOCK_WAIT=$BUILD_LOCK_WAIT" "${envs[@]}" bash "$runner" ci "$name" "$ts")

if [ "$action" = detach ]; then
  # Refuse early when the global CI lock or this worktree lock is taken (prints who holds it).
  rl_ssh_bash 'logs="$1"; name="$2"
    for l in ".lock-wt-$name" .lock-ci-global; do
      if ! flock -n "$logs/$l" true 2>/dev/null; then
        echo "remote-ci: busy ($l) held by: $(cat "$logs/$l.holder" 2>/dev/null || echo unknown)" >&2; exit 75
      fi
    done' "$logs" "$name" || exit $?
  rl_ssh_bash 'unit="$1"; shift
    export XDG_RUNTIME_DIR="/run/user/$(id -u)"
    if command -v systemd-run >/dev/null 2>&1 && systemd-run --user --collect --quiet --unit "$unit" "$@" 2>/dev/null; then
      echo "started with systemd-run --user: $unit"
    else
      nohup setsid "$@" >/dev/null 2>&1 </dev/null &
      echo "started with nohup (pid $!)"
    fi' "rusty-ci-$name-$ts" "${cmd[@]}" || exit $?
  echo "remote-ci: detached. log on server: $log"
  echo "remote-ci: inspect with: bash scripts/remote-ci.sh --status | --tail | --follow | --wait"
  exit 0
fi

echo "remote-ci: log on server: $log" >&2
rl_ssh "$(rl_quote "${cmd[@]}")"
rc=$?
echo "remote-ci: exit code $rc; log on $BUILD_HOST: $log" >&2
exit "$rc"
