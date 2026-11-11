#!/usr/bin/env bash
# The repository gate scripts and their self-tests, for the GitHub workflow (ci.yml calls this so
# the workflow files do not name the gated features). Same checks and order as scripts/ci.sh.
#   ci-gates.sh static            source-tree gates
#   ci-gates.sh symbols TARGET    release-build symbol gates for the given target directory name
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
case "${1:-}" in
  static)
    bash scripts/tests/remote-lib.sh
    bash scripts/tests/ci-changes.sh
    bash scripts/tests/ci-cache-prune.sh
    bash scripts/tests/ci-tool.sh
    bash scripts/tests/raw-db-gate.sh
    bash scripts/check-raw-db.sh
    bash scripts/tests/test-fixtures-gate.sh
    bash scripts/check-test-fixtures.sh static
    bash scripts/tests/mutation-hook-gate.sh
    bash scripts/check-mutation-hook.sh static
    bash scripts/tests/parametrised-gate.sh
    bash scripts/check-parametrised.sh
    bash scripts/tests/pg-migration-gap.sh
    bash scripts/check-pg-migration-gap.sh
    bash scripts/tests/wrapped-libs-gate.sh
    bash scripts/check-wrapped-libs.sh
    bash scripts/tests/service-crates-gate.sh
    bash scripts/check-service-crates.sh
    bash scripts/tests/test-knobs-gate.sh
    bash scripts/check-test-knobs.sh static
    bash scripts/tests/docker-pins-gate.sh
    bash scripts/check-docker-pins.sh
    ;;
  symbols)
    target="${2:?usage: ci-gates.sh symbols TARGET}"
    bash scripts/check-test-knobs.sh symbols "$target"
    bash scripts/check-test-fixtures.sh symbols "$target"
    bash scripts/check-mutation-hook.sh symbols "$target"
    ;;
  *)
    echo "usage: ci-gates.sh static | symbols TARGET" >&2
    exit 2
    ;;
esac
