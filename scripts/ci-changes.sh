#!/usr/bin/env bash
# Decide which CI jobs a change needs, from `git diff` only (plain shell, no third-party action).
#   ci-changes.sh                 diff HEAD^1..HEAD (the merge commit GitHub checks out for a pull
#                                 request: first parent = the base branch tip)
#   ci-changes.sh --stdin         read the changed paths (one per line) from standard input
# Prints  code=  rust=  docker=  workflow=  (true|false), one per line, for $GITHUB_OUTPUT.
#   code      anything that is not documentation or repository metadata: gates, lint, supply chain
#   rust      the Cargo workspace, its tests, the gate scripts, deploy and fuzz inputs
#   docker    the image: the workspace, deploy/, the image scripts and docker.yml itself
#   workflow  .github/workflows/docker.yml changed (a PR then also exercises the publish action)
# Fail-safe: everything is true unless the event is a pull request AND the diff could be read AND
# every changed path is positively classified. Pushes, tags, schedules and manual runs always run all
# jobs (they seed the caches and are the release gate). Test hook: RUSTY_CI_EVENT overrides
# GITHUB_EVENT_NAME.
set -euo pipefail

all_true() { printf 'code=true\nrust=true\ndocker=true\nworkflow=true\n'; }

# Paths that need no build or test. The one document a test reads is NOT here.
is_docs() {
  case "$1" in
    docs/superpowers/plans/2026-11-03-postgres-backend-mutation-table.md) return 1 ;;
    .github/ISSUE_TEMPLATE/*|.github/pull_request_template.md|.github/CODEOWNERS|.github/rulesets/*|.github/dependabot.yml) return 0 ;;
    docs/*|LICENSE|*.md) case "$1" in crates/*|tools/*|fuzz/*|deploy/*) return 1 ;; *) return 0 ;; esac ;;
    *) return 1 ;;
  esac
}

# Paths that cannot change what cargo builds or tests (they still trigger the gates and lint).
is_not_rust() {
  case "$1" in
    deploy/docker/*|deploy/images.lock|.dockerignore|.trivyignore) return 0 ;;
    .github/workflows/docker.yml|.github/workflows/release.yml) return 0 ;;
    scripts/check-image.sh|scripts/check-docker-pins.sh|scripts/tests/docker-pins-gate.sh) return 0 ;;
    *) return 1 ;;
  esac
}

is_image() {
  case "$1" in
    deploy/*|crates/*|tools/*|Cargo.toml|Cargo.lock|rust-toolchain.toml|.cargo/*|.dockerignore|.trivyignore) return 0 ;;
    scripts/check-image.sh|scripts/check-docker-pins.sh|scripts/gen-test-certs.sh|scripts/lib-probe-binary.sh) return 0 ;;
    .github/workflows/docker.yml) return 0 ;;
    *) return 1 ;;
  esac
}

classify() { # reads paths on stdin
  local code=false rust=false docker=false workflow=false p n=0
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    n=$((n + 1))
    [ "$p" != .github/workflows/docker.yml ] || workflow=true
    if is_docs "$p"; then continue; fi
    code=true
    is_not_rust "$p" || rust=true
    ! is_image "$p" || docker=true
  done
  if [ "$n" -eq 0 ]; then all_true; return; fi
  printf 'code=%s\nrust=%s\ndocker=%s\nworkflow=%s\n' "$code" "$rust" "$docker" "$workflow"
}

if [ "${1:-}" = "--stdin" ]; then classify; exit 0; fi
[ "$#" -eq 0 ] || { echo "usage: ci-changes.sh [--stdin]" >&2; exit 2; }

event="${RUSTY_CI_EVENT:-${GITHUB_EVENT_NAME:-}}"
if [ "$event" != pull_request ]; then all_true; exit 0; fi
if ! files="$(git diff --name-only --no-renames HEAD^1 HEAD 2>/dev/null)"; then
  echo "ci-changes: cannot read the diff; running everything" >&2
  all_true; exit 0
fi
printf '%s\n' "$files" | classify
