#!/usr/bin/env bash
# Script test: scripts/remote-lib.sh and the remote-*.sh / ci-run.sh front ends, with a fake
# ssh/rsync on PATH (no server needed): env loading, argument parsing, path safety, lock naming,
# exit-code propagation, detach and status handling.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
fail() { echo "remote-lib: $*" >&2; exit 1; }

# --- PATH shims -------------------------------------------------------------------------------
mkdir -p "$tmp/bin"
cat > "$tmp/bin/ssh" <<'SH'
#!/usr/bin/env bash
{ echo "--- ssh"; printf '%s\n' "$@"; } >> "$FAKE_LOG"
[ -t 0 ] || cat >/dev/null
case "$*" in
  *remote-runner*) exit "${FAKE_RUNNER_RC:-0}" ;;
  *latest-*) [ -z "${FAKE_STATE:-}" ] || echo "$FAKE_STATE" ;;
esac
exit 0
SH
cat > "$tmp/bin/rsync" <<'SH'
#!/usr/bin/env bash
{ echo "--- rsync"; printf '%s\n' "$@"; } >> "$FAKE_LOG"
SH
chmod +x "$tmp/bin/ssh" "$tmp/bin/rsync"
export FAKE_LOG="$tmp/log"
PATH="$tmp/bin:$PATH"

# --- config files -----------------------------------------------------------------------------
printf 'BUILD_HOST=10.0.0.5\r\n# c\nBUILD_KEY="C:\\keys\\k"\nBUILD_PORT=2222\nEVIL=1\n' > "$tmp/env"
echo "10.0.0.5 ssh-ed25519 AAAA" > "$tmp/kh"
export BUILD_KNOWN_HOSTS="$tmp/kh" RUSTY_REMOTE_ENV="$tmp/env" RL_WORKTREE_NAME="wt-test"
unset BUILD_HOST BUILD_USER BUILD_KEY BUILD_PORT BUILD_MUX BUILD_SYNC BUILD_LOCK_WAIT EVIL

# 1. env loading: file values, CRLF, quotes, backslashes, defaults, unknown keys ignored
(
  . scripts/remote-lib.sh; rl_load_env
  [ "$BUILD_HOST" = 10.0.0.5 ] || fail "host from file: $BUILD_HOST"
  [ "$BUILD_KEY" = "C:/keys/k" ] || fail "key normalisation: $BUILD_KEY"
  [ "$BUILD_PORT" = 2222 ] && [ "$BUILD_USER" = rustybuild ] || fail "port/user default"
  [ -z "${EVIL:-}" ] || fail "unknown key was imported"
  :
) || exit 1
# environment wins over the file
(
  export BUILD_HOST=9.9.9.9; . scripts/remote-lib.sh; rl_load_env
  [ "$BUILD_HOST" = 9.9.9.9 ] || fail "env must override file"
  :
) || exit 1

# 2. validation: missing/invalid configuration is refused
(
  . scripts/remote-lib.sh; RUSTY_REMOTE_ENV="$tmp/none"; rl_load_env
  rcout="$( (rl_require_config) 2>&1)"; rc=$?
  [ "$rc" -eq 2 ] && [[ "$rcout" == *BUILD_HOST* ]] || fail "missing host not refused ($rc)"
  BUILD_HOST='a;b' BUILD_KEY=k; (rl_require_config) >/dev/null 2>&1 && fail "bad host accepted"
  BUILD_HOST=h BUILD_KEY=k BUILD_PORT=x; (rl_require_config) >/dev/null 2>&1 && fail "bad port accepted"
  BUILD_PORT=22 BUILD_SYNC=ftp; (rl_require_config) >/dev/null 2>&1 && fail "bad sync accepted"
  :
) || exit 1

# 3. worktree name and remote path safety
(
  . scripts/remote-lib.sh
  for bad in "a b" ".." "../x" "a/b" "-rf" 'a;b' 'a$b' "x..y"; do
    RL_WORKTREE_NAME="$bad"; rl_worktree_name >/dev/null 2>&1 && fail "unsafe name accepted: '$bad'"
  done
  RL_WORKTREE_NAME="Rusty-imap.2"; [ "$(rl_worktree_name)" = "Rusty-imap.2" ] || fail "good name refused"
  for ok in /srv/rusty/work/wt /srv/rusty/target/wt-1 /srv/rusty/work/wt/sub/dir; do
    rl_safe_remote_path "$ok" || fail "safe path refused: $ok"
  done
  for bad in / /srv /srv/rusty /srv/rusty/work /srv/rusty/work/ /srv/rusty/work/.. /srv/rusty/work/../etc \
             /srv/rusty/work/a/../../etc /etc/passwd /srv/rusty/logs/x '/srv/rusty/work/a b' \
             '/srv/rusty/work/a;rm' '/srv/rusty/work/$HOME' /srv/rusty/work//x relative/work/x /home/x; do
    rl_safe_remote_path "$bad" && fail "unsafe path accepted: $bad"
  done
  :
) || exit 1

# 4. lock naming
(
  . scripts/remote-lib.sh
  [ "$(rl_lock_name wt foo)" = ".lock-wt-foo" ] || fail "wt lock name"
  [ "$(rl_lock_name ci)" = ".lock-ci-global" ] || fail "ci lock name"
  [ "$(rl_lock_name wt foo)" != "$(rl_lock_name wt bar)" ] || fail "locks not per worktree"
  rl_lock_name wt "../x" >/dev/null 2>&1 && fail "unsafe lock name accepted"
  rl_lock_name other >/dev/null 2>&1 && fail "unknown lock kind accepted"
  :
) || exit 1

# 5. remote-cargo: ssh hardening options, quoting, exit code, sync modes
: > "$FAKE_LOG"
BUILD_SYNC=tar bash scripts/remote-cargo.sh test --workspace -- "a b" >/dev/null 2>"$tmp/err"; rc=$?
[ "$rc" -eq 0 ] || fail "remote-cargo rc=$rc: $(cat "$tmp/err")"
for want in BatchMode=yes StrictHostKeyChecking=yes "UserKnownHostsFile=$tmp/kh" ServerAliveInterval=15 ConnectTimeout=10 2222 "C:/keys/k"; do
  grep -qxF -- "$want" "$FAKE_LOG" || fail "ssh option missing: $want"
done
grep -q 'remote-runner.sh' "$FAKE_LOG" && grep -q 'wt-test' "$FAKE_LOG" || fail "runner command missing"
grep -Eq 'a\\ b|"a b"|a.b' "$FAKE_LOG" || fail "argument with space lost"
grep -q ' test --workspace -- a\\ b' "$FAKE_LOG" || fail "cargo arguments not quoted exactly"
grep -q 'tar over ssh' "$tmp/err" || fail "tar fallback message missing"
FAKE_RUNNER_RC=7 BUILD_SYNC=tar bash scripts/remote-cargo.sh build >/dev/null 2>&1; rc=$?
[ "$rc" -eq 7 ] || fail "exit code not propagated (got $rc)"
: > "$FAKE_LOG"
BUILD_SYNC=rsync bash scripts/remote-sync.sh >/dev/null 2>&1 || fail "rsync sync failed"
grep -qxF -- "--delete" "$FAKE_LOG" || fail "rsync without --delete"
grep -qF -- "10.0.0.5" "$FAKE_LOG" && grep -q '/srv/rusty/work/wt-test/$' "$FAKE_LOG" || fail "rsync destination wrong"
grep -qxF -- "/target/" "$FAKE_LOG" || fail "target/ not excluded"
# no config -> clear refusal, no ssh call
: > "$FAKE_LOG"
( unset BUILD_KNOWN_HOSTS; RUSTY_REMOTE_ENV="$tmp/none" bash scripts/remote-cargo.sh --version ) >/dev/null 2>&1 && fail "ran without configuration"
! grep -q -- '--- ssh' "$FAKE_LOG" || fail "ssh called without configuration"
# unpinned host key -> refused
( BUILD_KNOWN_HOSTS="$tmp/nokh" bash scripts/remote-cargo.sh --version ) >/dev/null 2>&1 && fail "ran without a pinned host key"

# 6. remote-ci: detach, status codes, bad arguments
: > "$FAKE_LOG"
BUILD_SYNC=tar bash scripts/remote-ci.sh --detach >"$tmp/out" 2>&1 || fail "detach failed: $(cat "$tmp/out")"
grep -q 'systemd-run' "$FAKE_LOG" && grep -q 'nohup' "$FAKE_LOG" || fail "detach does not use systemd-run/nohup"
grep -q 'rusty-ci-wt-test-' "$FAKE_LOG" || fail "unit name missing"
grep -q '/srv/rusty/logs/wt-test-.*-ci.log' "$tmp/out" || fail "log path not printed"
grep -q '\.lock-ci-global' "$FAKE_LOG" || fail "global CI lock not probed"
FAKE_STATE="STATE=finished RC=3 LOG=/x" bash scripts/remote-ci.sh --status >/dev/null 2>&1; [ $? -eq 3 ] || fail "status finished rc"
FAKE_STATE="STATE=running RC= LOG=/x" bash scripts/remote-ci.sh --status >/dev/null 2>&1; [ $? -eq 100 ] || fail "status running"
FAKE_STATE="STATE=none RC= LOG=" bash scripts/remote-ci.sh --status >/dev/null 2>&1; [ $? -eq 101 ] || fail "status none"
FAKE_STATE="STATE=died RC= LOG=/x" bash scripts/remote-ci.sh --status >/dev/null 2>&1; [ $? -eq 102 ] || fail "status died"
FAKE_STATE="STATE=finished RC=0 LOG=/x" bash scripts/remote-ci.sh --wait >/dev/null 2>&1 || fail "wait on finished rc 0"
FAKE_STATE="STATE=finished RC=5 LOG=/x" bash scripts/remote-ci.sh --wait >/dev/null 2>&1; [ $? -eq 5 ] || fail "wait rc"
bash scripts/remote-ci.sh --bogus >/dev/null 2>&1; [ $? -eq 2 ] || fail "unknown argument not refused"
FAKE_RUNNER_RC=75 BUILD_SYNC=tar bash scripts/remote-ci.sh >/dev/null 2>&1; [ $? -eq 75 ] || fail "busy rc not propagated"

# 7. dispatcher: remote when the env file exists, else WSL
mkdir -p "$tmp/d/scripts"
cp scripts/ci-run.sh "$tmp/d/scripts/"
echo 'echo REMOTE "$@"' > "$tmp/d/scripts/remote-ci.sh"
echo 'echo WSL "$@"' > "$tmp/d/scripts/wsl-ci.sh"
[ "$(RUSTY_REMOTE_ENV="$tmp/d/absent" bash "$tmp/d/scripts/ci-run.sh")" = "WSL" ] || fail "dispatcher: no env file must pick WSL"
[ "$(RUSTY_REMOTE_ENV="$tmp/env" bash "$tmp/d/scripts/ci-run.sh" --detach)" = "REMOTE --detach" ] || fail "dispatcher: env file must pick remote"
[ "$(RUSTY_BUILD=wsl RUSTY_REMOTE_ENV="$tmp/env" bash "$tmp/d/scripts/ci-run.sh")" = "WSL" ] || fail "dispatcher: RUSTY_BUILD override"

# 8. the server-side runner refuses bad input
bash scripts/remote-runner.sh ci '../x' 20260101-000000 >/dev/null 2>&1; [ $? -eq 2 ] || fail "runner accepted a bad name"
bash scripts/remote-runner.sh rm wt 20260101-000000 >/dev/null 2>&1; [ $? -eq 2 ] || fail "runner accepted a bad mode"
bash scripts/remote-runner.sh ci wt "x; id" >/dev/null 2>&1; [ $? -eq 2 ] || fail "runner accepted a bad timestamp"

echo "remote-lib: OK"
