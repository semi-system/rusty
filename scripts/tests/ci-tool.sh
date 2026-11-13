#!/usr/bin/env bash
# Self-test of scripts/ci-tool.sh with a fake installer: pinned versions are required, a matching
# recorded version skips the install, a changed pin reinstalls.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
fail() { echo "ci-tool self-test: $*" >&2; exit 1; }
printf '#!/usr/bin/env bash\necho "$@" >> "%s/calls"\n' "$tmp" > "$tmp/fake"; chmod +x "$tmp/fake"
export CI_TOOLS_ROOT="$tmp/root" CI_TOOL_INSTALL_CMD="$tmp/fake"
bash "$here/scripts/ci-tool.sh" cargo-deny >/dev/null
grep -q -- "--locked --root $tmp/root --version 0.20.0 cargo-deny" "$tmp/calls" || fail "install not called with the pinned version: $(cat "$tmp/calls")"
bash "$here/scripts/ci-tool.sh" cargo-deny | grep -q 'cache hit' || fail "second run must be a cache hit"
[ "$(wc -l < "$tmp/calls")" -eq 1 ] || fail "installed twice"
echo 0.1.0 > "$tmp/root/versions/cargo-deny"
bash "$here/scripts/ci-tool.sh" cargo-deny >/dev/null; [ "$(wc -l < "$tmp/calls")" -eq 2 ] || fail "version change must reinstall"
bash "$here/scripts/ci-tool.sh" cargo-nope >/dev/null 2>&1 && fail "unpinned tool accepted"
( unset CI_TOOLS_ROOT; bash "$here/scripts/ci-tool.sh" cargo-deny >/dev/null 2>&1 ) && fail "ran without CI_TOOLS_ROOT"
for t in cargo-deny cargo-audit cargo-fuzz; do grep -qE "^$t [0-9]+\.[0-9]+\.[0-9]+$" "$here/scripts/ci-tools.txt" || fail "$t not pinned"; done
echo "ci-tool self-test OK"
