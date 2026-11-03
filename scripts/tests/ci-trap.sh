#!/usr/bin/env bash
# Script test: a forced failure inside a stage must be reported by name by the permanent EXIT
# trap of ci.sh. RUSTY_CI_FORCE_FAIL=<stage> replaces that stage's command with `false`; the
# hook exists for this test only.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
out="$(RUSTY_CI_FORCE_FAIL=clippy bash scripts/ci.sh 2>&1)"
rc=$?
if [ "$rc" -eq 0 ]; then
  echo "ci-trap: ci.sh succeeded although clippy was forced to fail" >&2; exit 1
fi
if ! grep -q '^\[FAIL\] ci.sh exited with [1-9][0-9]* during clippy$' <<<"$out"; then
  echo "ci-trap: the EXIT trap did not name the failing stage; output was:" >&2
  echo "$out" >&2
  exit 1
fi
echo "ci-trap: OK (exit $rc, stage named)"
