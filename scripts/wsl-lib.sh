#!/usr/bin/env bash
# Shared helpers for wsl-cargo.sh and wsl-ci.sh (sourced, not run).
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL="*"

command -v wsl.exe >/dev/null 2>&1 || { echo "error: wsl.exe not found; WSL is required" >&2; exit 127; }
wsl.exe -e true >/dev/null 2>&1 || { echo "error: WSL is not available or failed to start" >&2; exit 127; }

REPO_WIN="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -W 2>/dev/null || pwd)"
REPO_WSL="$(wsl.exe -e wslpath -a "$REPO_WIN" | tr -d '\r')"
[ -n "$REPO_WSL" ] || { echo "error: wslpath failed for $REPO_WIN" >&2; exit 127; }

# wsl_run <command> [args...]: run in the repo dir inside WSL. Arguments are passed
# positionally to "bash -c" and expanded as "$@", so no quoting is ever re-parsed.
export WSLENV="${WSLENV:+$WSLENV:}RUSTY_WSL_TARGET_DIR"

wsl_run() {
  wsl.exe -e bash -lc '
    d="$1"; shift
    export CARGO_TARGET_DIR="${RUSTY_WSL_TARGET_DIR:-${CARGO_TARGET_DIR:-$HOME/rusty-target}}"
    cd "$d" || exit 127
    exec "$@"
  ' wsl-run "$REPO_WSL" "$@"
}
