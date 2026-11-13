#!/usr/bin/env bash
# Lint the GitHub workflows (actionlint) and every shell script under scripts/, deploy/ and
# tools/acceptance (shellcheck), each in a container pinned by digest from deploy/images.lock.
# Usage: ci-workflows-lint.sh [ROOT]   (default: the repository root)
# Without Docker the check is SKIPPED with an explicit note (like the other Docker stages of
# ci.sh); with Docker any finding fails. Test hook: RUSTY_LINT_NO_DOCKER=1 behaves like no Docker.
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

if [ "${RUSTY_LINT_NO_DOCKER:-}" = 1 ] || ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "NOTE: Docker is unavailable; actionlint/shellcheck were NOT run (workflows and scripts are unlinted)." >&2
  exit 0
fi

LOCK="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/deploy/images.lock"
ref() { awk -v n="$1" '$1 == n { print $2 }' "$LOCK"; }
ACTIONLINT="$(ref rhysd/actionlint:1.7.7)"
SHELLCHECK="$(ref koalaman/shellcheck:v0.10.0)"
[ -n "$ACTIONLINT" ] && [ -n "$SHELLCHECK" ] || { echo "FAIL: lint images are not pinned in deploy/images.lock" >&2; exit 1; }

rc=0
if [ -d .github/workflows ]; then
  echo "-- actionlint"
  # Files are passed explicitly: actionlint otherwise needs a .git directory to find the project
  # (absent in the synced copy on the build server and in the read-only mount).
  mapfile -t wfs < <(find .github/workflows -maxdepth 1 -type f \( -name '*.yml' -o -name '*.yaml' \) | LC_ALL=C sort)
  docker run --rm --network none -v "$PWD:/repo:ro" -w /repo "$ACTIONLINT" -color=false "${wfs[@]}" || rc=1
fi

echo "-- shellcheck"
mapfile -t files < <(
  { find scripts deploy tools/acceptance -type f -name '*.sh' 2>/dev/null; } | LC_ALL=C sort
)
if [ "${#files[@]}" -gt 0 ]; then
  docker run --rm --network none -v "$PWD:/mnt:ro" -w /mnt "$SHELLCHECK" -x -S warning "${files[@]}" || rc=1
fi
if [ "$rc" = 0 ]; then echo "workflows and scripts lint clean (${#files[@]} shell files)"; fi
exit "$rc"
