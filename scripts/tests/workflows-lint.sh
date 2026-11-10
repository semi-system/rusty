#!/usr/bin/env bash
# Self-test of scripts/ci-workflows-lint.sh: a clean scratch tree passes, a broken workflow and a
# broken shell script each fail, and without Docker the check skips with the explicit note.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mk() {
  rm -rf "$tmp/t"; mkdir -p "$tmp/t/.github/workflows" "$tmp/t/scripts"
  printf 'name: t\non: [push]\npermissions: {}\njobs:\n  a:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: echo hi\n' > "$tmp/t/.github/workflows/a.yml"
  printf '#!/usr/bin/env bash\nset -euo pipefail\necho "$1"\n' > "$tmp/t/scripts/a.sh"
}
expect() { local want="$2" rc=0; shift 2; "$@" >"$tmp/out" 2>&1 || rc=$?; [ "$rc" = "$want" ] || { echo "FAIL: $1 (rc=$rc want=$want)" >&2; cat "$tmp/out" >&2; exit 1; }; }
lint() { bash "$here/scripts/ci-workflows-lint.sh" "$tmp/t"; }

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  mk; expect "no docker skips" 0 lint; grep -q "NOT run" "$tmp/out" || { echo "FAIL: no skip note" >&2; exit 1; }
  echo "workflows-lint self-test: only the skip path ran (no Docker)"; exit 0
fi
mk; expect "clean tree" 0 lint
mk; sed -i 's/runs-on: ubuntu-24.04/runs-on: ubuntu-24.04\n    needs: nothing/' "$tmp/t/.github/workflows/a.yml"; expect "unknown job in needs" 1 lint
mk; sed -i 's/echo hi/echo ${{ github.evnt.name }}/' "$tmp/t/.github/workflows/a.yml"; expect "unknown context property" 1 lint
mk; printf 'cat $1 | rm $2\nfor f in $(ls); do echo $f; done\n' >> "$tmp/t/scripts/a.sh"; expect "shellcheck finding" 1 lint
mk; expect "forced skip" 0 env RUSTY_LINT_NO_DOCKER=1 bash "$here/scripts/ci-workflows-lint.sh" "$tmp/t"
grep -q "NOT run" "$tmp/out" || { echo "FAIL: no skip note" >&2; exit 1; }
echo "workflows-lint self-test OK"
