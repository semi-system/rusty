#!/usr/bin/env bash
# Run cargo inside WSL for this repo, from Git Bash on Windows:
#   bash scripts/wsl-cargo.sh test --workspace
# The target dir lives on the WSL ext4 disk (fast); override with RUSTY_WSL_TARGET_DIR.
set -euo pipefail

# shellcheck source=/dev/null
. "$(dirname "${BASH_SOURCE[0]}")/wsl-lib.sh"
wsl_run cargo "$@"
