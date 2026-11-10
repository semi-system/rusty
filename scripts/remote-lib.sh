#!/usr/bin/env bash
# Shared helpers for remote-cargo.sh, remote-ci.sh and remote-sync.sh (sourced, not run).
# They run the repo's build/CI on the dedicated Ubuntu build server over SSH. See docs/buildserver.md.
#
# Configuration (environment wins over the file): BUILD_HOST, BUILD_USER, BUILD_KEY, BUILD_PORT,
# BUILD_KNOWN_HOSTS, BUILD_MUX, BUILD_SYNC (auto|rsync|tar), BUILD_LOCK_WAIT.
# File: $RUSTY_REMOTE_ENV or <repo>/.rusty-remote.env (untracked; KEY=VALUE lines only, never executed).
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL="*"

RL_REMOTE_ROOT="/srv/rusty"
RL_ALLOWED_KEYS="BUILD_HOST BUILD_USER BUILD_KEY BUILD_PORT BUILD_KNOWN_HOSTS BUILD_MUX BUILD_SYNC BUILD_LOCK_WAIT"

rl_die() { echo "remote: $*" >&2; exit "${RL_RC:-2}"; }

# rl_repo_root: the worktree root (Windows form is fine for tools; we only need a usable path).
rl_repo_root() {
  if [ -n "${RL_REPO_ROOT:-}" ]; then printf '%s\n' "$RL_REPO_ROOT"; return; fi
  (cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
}

rl_env_file() { printf '%s\n' "${RUSTY_REMOTE_ENV:-$(rl_repo_root)/.rusty-remote.env}"; }

# rl_load_env: parse KEY=VALUE lines of the env file for allowed keys; variables already set in
# the environment are kept. Quotes around the value are stripped. Nothing is executed.
rl_load_env() {
  local f line key val
  f="$(rl_env_file)"
  if [ -f "$f" ]; then
    while IFS= read -r line || [ -n "$line" ]; do
      line="${line%$'\r'}"
      case "$line" in ''|'#'*) continue ;; esac
      [[ "$line" =~ ^[[:space:]]*([A-Z_]+)=(.*)$ ]] || continue
      key="${BASH_REMATCH[1]}"; val="${BASH_REMATCH[2]}"
      case " $RL_ALLOWED_KEYS " in *" $key "*) ;; *) continue ;; esac
      val="${val%\"}"; val="${val#\"}"; val="${val%\'}"; val="${val#\'}"
      [ -n "${!key:-}" ] || export "$key=$val"
    done < "$f"
  fi
  BUILD_USER="${BUILD_USER:-rustybuild}"
  BUILD_PORT="${BUILD_PORT:-22}"
  BUILD_SYNC="${BUILD_SYNC:-auto}"
  BUILD_LOCK_WAIT="${BUILD_LOCK_WAIT:-0}"
  if [ -n "${BUILD_KEY:-}" ]; then BUILD_KEY="${BUILD_KEY//\\//}"; fi
  if [ -z "${BUILD_KNOWN_HOSTS:-}" ]; then BUILD_KNOWN_HOSTS="$(dirname "$f")/.rusty-remote.known_hosts"; fi
  BUILD_KNOWN_HOSTS="${BUILD_KNOWN_HOSTS//\\//}"
  if [ -z "${BUILD_MUX:-}" ]; then
    case "$(uname -s 2>/dev/null)" in MINGW*|MSYS*|CYGWIN*) BUILD_MUX=0 ;; *) BUILD_MUX=1 ;; esac
  fi
  export BUILD_USER BUILD_PORT BUILD_SYNC BUILD_LOCK_WAIT BUILD_KNOWN_HOSTS BUILD_MUX
}

rl_require_config() {
  [ -n "${BUILD_HOST:-}" ] || rl_die "BUILD_HOST is not set; create $(rl_env_file) from .rusty-remote.env.example"
  [ -n "${BUILD_KEY:-}" ] || rl_die "BUILD_KEY is not set (path to the ed25519 private key)"
  [[ "$BUILD_HOST" =~ ^[A-Za-z0-9._:-]+$ ]] || rl_die "BUILD_HOST has invalid characters"
  [[ "$BUILD_USER" =~ ^[a-z_][a-z0-9_-]*$ ]] || rl_die "BUILD_USER has invalid characters"
  [[ "$BUILD_PORT" =~ ^[0-9]{1,5}$ ]] || rl_die "BUILD_PORT must be a number"
  [[ "$BUILD_LOCK_WAIT" =~ ^[0-9]{1,5}$ ]] || rl_die "BUILD_LOCK_WAIT must be a number of seconds"
  case "$BUILD_SYNC" in auto|rsync|tar) ;; *) rl_die "BUILD_SYNC must be auto, rsync or tar" ;; esac
}

# rl_worktree_name: basename of the worktree, restricted to a safe charset.
rl_worktree_name() {
  local n
  n="${RL_WORKTREE_NAME:-$(basename "$(rl_repo_root)")}"
  [[ "$n" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]] || { echo "remote: unsafe worktree name '$n'" >&2; return 2; }
  case "$n" in *..*) echo "remote: unsafe worktree name '$n'" >&2; return 2 ;; esac
  printf '%s\n' "$n"
}

# rl_safe_remote_path <path>: accept only /srv/rusty/{work,target}/<name>[/sub], no "..".
rl_safe_remote_path() {
  local p="$1"
  case "$p" in *..*|*//*|*[[:space:]]*|*\'*|*\"*|*\$*|*\`*|*\;*|*\\*) return 1 ;; esac
  [[ "$p" =~ ^/srv/rusty/(work|target)/[A-Za-z0-9][A-Za-z0-9._-]{0,63}(/[A-Za-z0-9._/-]*)?$ ]]
}

# Lock file names (in /srv/rusty/logs): one per worktree, one global for the full CI.
rl_lock_name() { # wt <name> | ci
  case "$1" in
    wt) [[ "${2:-}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]] || return 2; printf '.lock-wt-%s\n' "$2" ;;
    ci) printf '.lock-ci-global\n' ;;
    *) return 2 ;;
  esac
}

rl_work_dir() { printf '%s/work/%s\n' "$RL_REMOTE_ROOT" "$1"; }
rl_target_dir() { printf '%s/target/%s\n' "$RL_REMOTE_ROOT" "$1"; }
rl_logs_dir() { printf '%s/logs\n' "$RL_REMOTE_ROOT"; }

# SSH ---------------------------------------------------------------------------------------
rl_ssh_args() { # fills RL_SSH_ARGS
  RL_SSH_ARGS=(-p "$BUILD_PORT" -i "$BUILD_KEY" -o IdentitiesOnly=yes -o BatchMode=yes
    -o StrictHostKeyChecking=yes -o "UserKnownHostsFile=$BUILD_KNOWN_HOSTS"
    -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4
    -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no)
  if [ "$BUILD_MUX" = 1 ]; then
    RL_SSH_ARGS+=(-o ControlMaster=auto -o ControlPersist=120 -o "ControlPath=${TMPDIR:-/tmp}/rusty-cm-%C")
  fi
}

rl_ssh() { # rl_ssh <remote command string>
  rl_ssh_args
  ssh "${RL_SSH_ARGS[@]}" "$BUILD_USER@$BUILD_HOST" "$@"
}

# rl_quote args...: shell-quote each argument for the remote shell.
rl_quote() { local a out=""; for a in "$@"; do out+="$(printf '%q' "$a") "; done; printf '%s' "${out% }"; }

# rl_ssh_bash <script> [args...]: run "bash -c script _ args" remotely with exact quoting.
rl_ssh_bash() {
  local script="$1"; shift
  rl_ssh "bash -c $(rl_quote "$script") _ $(rl_quote "$@")"
}

rl_check_known_hosts() {
  [ -s "$BUILD_KNOWN_HOSTS" ] || rl_die "no pinned host key in $BUILD_KNOWN_HOSTS; run: bash scripts/remote-sync.sh --pin-host"
}

# Sync --------------------------------------------------------------------------------------
RL_EXCLUDES=(target/ .git/ fuzz/artifacts/ fuzz/target/ node_modules/ .rusty-remote.env .rusty-remote.known_hosts)

rl_excluded() { # path -> 0 if excluded
  local p="$1" e
  for e in "${RL_EXCLUDES[@]}"; do
    case "$e" in
      */) case "$p" in "$e"*|*"/$e"*) return 0 ;; esac ;;
      *) [ "$p" = "$e" ] && return 0 ;;
    esac
  done
  return 1
}

# rl_file_list: NUL-separated list of tracked + untracked-not-ignored files that exist now.
rl_file_list() {
  local f
  (cd "$(rl_repo_root)" && git ls-files -z --cached --others --exclude-standard) |
    while IFS= read -r -d '' f; do
      rl_excluded "$f" && continue
      [ -e "$(rl_repo_root)/$f" ] || continue
      printf '%s\0' "$f"
    done
}

rl_pick_sync() {
  case "$BUILD_SYNC" in
    rsync) command -v rsync >/dev/null 2>&1 || rl_die "BUILD_SYNC=rsync but rsync is not installed locally"; echo rsync ;;
    tar) echo tar ;;
    *) if command -v rsync >/dev/null 2>&1; then echo rsync; else echo tar; fi ;;
  esac
}

# rl_sync: copy the worktree (incl. uncommitted changes) to /srv/rusty/work/<name>/.
rl_sync() {
  local name root dir stage mode
  name="$(rl_worktree_name)" || exit 2
  root="$(rl_repo_root)"
  dir="$(rl_work_dir "$name")"; stage="$RL_REMOTE_ROOT/work/${name}.stage"
  rl_safe_remote_path "$dir" || rl_die "refusing unsafe remote path: $dir"
  mode="$(rl_pick_sync)"
  rl_ssh "mkdir -p $(rl_quote "$dir") $(rl_quote "$(rl_logs_dir)") $(rl_quote "$(rl_target_dir "$name")")"
  if [ "$mode" = rsync ]; then
    local e opts=() ex
    rl_ssh_args
    e="ssh $(rl_quote "${RL_SSH_ARGS[@]}")"
    for ex in "${RL_EXCLUDES[@]}"; do opts+=(--exclude "/$ex" --exclude "$ex"); done
    # --delete only ever applies inside $dir (the destination), which was validated above.
    rsync -rlptz --delete --delete-excluded --filter=':- .gitignore' "${opts[@]}" \
      -e "$e" "$root/" "$BUILD_USER@$BUILD_HOST:$dir/"
  else
    echo "remote: rsync not available locally; syncing with tar over ssh (git file list)" >&2
    rl_safe_remote_path "$stage" || rl_die "refusing unsafe staging path: $stage"
    rl_file_list | (cd "$root" && tar --null -T - -cf -) |
      rl_ssh_bash 'set -e; stage="$1"; dir="$2"
        rm -rf "$stage"; mkdir -p "$stage"; tar -xf - -C "$stage" --no-same-owner
        rsync -rlpt --delete "$stage/" "$dir/"; rm -rf "$stage"' "$stage" "$dir"
  fi
  echo "remote: synced $name -> $BUILD_USER@$BUILD_HOST:$dir ($mode)" >&2
}

# Env vars forwarded to the remote run (allow-list).
rl_forward_env() {
  local v out=()
  for v in FUZZ_SECS EDGE_ONLY EDGE_KEEP EDGE_REPORT EDGE_SUITE RUSTY_REQUIRE_PG RUSTY_PG_RUN_ID; do
    [ -z "${!v:-}" ] || out+=("$v=${!v}")
  done
  [ "${#out[@]}" -eq 0 ] || printf '%s\n' "${out[@]}"
}

rl_timestamp() { date +%Y%m%d-%H%M%S; }
