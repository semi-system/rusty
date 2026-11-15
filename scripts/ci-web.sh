#!/bin/sh
# The front end gates (design section 8): install from the lockfile without running any package script, then the supply-chain, type, lint, unit,
# build, bundle and reproducibility checks. Needs Node and npm on PATH (POSIX sh: the Node image has no bash);
# `scripts/ci-web-docker.sh` runs it in the image pinned in deploy/images.lock.
set -eu
cd "$(dirname "$0")/../web/admin"

step() { printf '\n== web: %s\n' "$*"; }

step "npm ci --ignore-scripts (lockfile only, no package script runs)"
npm ci --ignore-scripts --no-audit --no-fund
step "dependency allow-list, integrity, licences"
node scripts/check-deps.mjs
step "types generated from the contract are current"
node scripts/gen-types.mjs --check
step "svelte-check"
npx svelte-check --tsconfig ./tsconfig.json --fail-on-warnings
step "eslint (no {@html}, no literal text, no eval)"
npx eslint . --max-warnings 0
step "vitest"
npx vitest run
step "build, bundle scan, reproducibility"
sh scripts/check-reproducible.sh
node scripts/check-bundle.mjs
step "npm audit"
# The shipped bundle has no runtime dependency, so the production tree must be clean; the tooling tree is reported and fails only on a critical advisory.
npm audit --omit=dev --audit-level=low
npm audit --audit-level=critical || { echo "critical advisory in the build tooling" >&2; exit 1; }
printf '\nweb gates passed\n'
