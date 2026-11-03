#!/usr/bin/env bash
# Run the CI gate inside WSL: bash scripts/wsl-ci.sh
# Full log: $HOME/rusty-ci.log (WSL home, persistent). Summary lines and exit code go to stdout.
set -uo pipefail

# shellcheck source=/dev/null
. "$(dirname "${BASH_SOURCE[0]}")/wsl-lib.sh"

# tee runs inside WSL so the log survives; the exit code is saved to a file and
# echoed as a final marker line.
wsl_run bash -c '
  log="$HOME/rusty-ci.log"
  bash scripts/ci.sh 2>&1 | tee "$log"
  rc=${PIPESTATUS[0]}
  echo "CI_EXIT_CODE=$rc" >> "$log"
  exit "$rc"
' ci-wrapper 2>&1 | tee "${TMPDIR:-/tmp}/wsl-ci.$$.out"
rc=${PIPESTATUS[0]}
out="${TMPDIR:-/tmp}/wsl-ci.$$.out"
echo "---- summary (full log: ~/rusty-ci.log in WSL) ----"
grep -E '^\[ OK \]|^\[FAIL\]|^CI OK|^WARNING' "$out" || true
echo "exit code: $rc"
rm -f "$out"
exit "$rc"
