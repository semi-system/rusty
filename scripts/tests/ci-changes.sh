#!/usr/bin/env bash
# Self-test of scripts/ci-changes.sh: path classification, the fail-safe defaults, and the git
# diff path against a scratch repository with a merge commit like the one a pull request checks out.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
fail() { echo "ci-changes self-test: $*" >&2; exit 1; }

classify() { printf '%s\n' "$@" | bash "$here/scripts/ci-changes.sh" --stdin | tr '\n' ' '; }
expect() { # "code rust docker workflow" then paths...
  local want="code=$1 rust=$2 docker=$3 workflow=$4 " got; shift 4
  got="$(classify "$@")"
  [ "$got" = "$want" ] || fail "paths '$*': got '$got' want '$want'"
}

expect false false false false docs/ci.md README.md CONTRIBUTING.md .github/pull_request_template.md .github/ISSUE_TEMPLATE/bug_report.yml .github/rulesets/main.json LICENSE
expect true  true  true  false crates/rusty-smtp/src/lib.rs
expect true  true  true  false Cargo.lock
expect true  true  false false fuzz/seeds/fuzz_pem/a            # fuzz seeds are read by tests; not in the image
expect true  true  false false deny.toml
expect true  true  false false scripts/ci.sh
expect true  true  false false .github/workflows/ci.yml
expect true  false true  true  .github/workflows/docker.yml
expect true  false true  false deploy/docker/Dockerfile
expect true  false false false .github/workflows/release.yml
expect true  true  true  true  docs/a.md .github/workflows/docker.yml crates/rusty-core/src/lib.rs
expect true  true  false false docs/superpowers/plans/2026-11-03-postgres-backend-mutation-table.md
expect true  true  true  false crates/rusty-core/README.md      # crate docs can be include_str!-ed
expect true  true  false false rusty.example.toml
# an empty list is not "nothing changed": fail safe
[ "$(printf '' | bash "$here/scripts/ci-changes.sh" --stdin | tr '\n' ' ')" = "code=true rust=true docker=true workflow=true " ] || fail "empty list must run everything"
# the git path
mkdir "$tmp/r"; cd "$tmp/r"
git init -q -b main . && git config user.email t@example.invalid && git config user.name t && git config core.autocrlf false
mkdir docs crates; echo a > docs/a.md; echo b > crates/b.rs; git add -A; git commit -q -m base
git checkout -q -b pr; echo c >> docs/a.md; git commit -qam docs
git checkout -q main; git merge -q --no-ff pr -m "Merge pr"   # HEAD^1 = base tip, like the PR merge ref
out="$(RUSTY_CI_EVENT=pull_request bash "$here/scripts/ci-changes.sh" | tr '\n' ' ')"
[ "$out" = "code=false rust=false docker=false workflow=false " ] || fail "docs-only merge: $out"
git checkout -q -b pr2; echo d >> crates/b.rs; git commit -qam code
git checkout -q main; git merge -q --no-ff pr2 -m "Merge pr2"
out="$(RUSTY_CI_EVENT=pull_request bash "$here/scripts/ci-changes.sh" | tr '\n' ' ')"
[ "$out" = "code=true rust=true docker=true workflow=false " ] || fail "code merge: $out"
# not a pull request: everything runs, even for a docs-only change
out="$(RUSTY_CI_EVENT=push bash "$here/scripts/ci-changes.sh" | tr '\n' ' ')"
[ "$out" = "code=true rust=true docker=true workflow=true " ] || fail "push must run everything: $out"
out="$(RUSTY_CI_EVENT=schedule bash "$here/scripts/ci-changes.sh" | tr '\n' ' ')"
[ "$out" = "code=true rust=true docker=true workflow=true " ] || fail "schedule must run everything: $out"
# unreadable diff (no parent): everything runs
git checkout -q --orphan lone; git commit -q --allow-empty -m lone
out="$(RUSTY_CI_EVENT=pull_request bash "$here/scripts/ci-changes.sh" 2>/dev/null | tr '\n' ' ')"
[ "$out" = "code=true rust=true docker=true workflow=true " ] || fail "unreadable diff must run everything: $out"
bash "$here/scripts/ci-changes.sh" --bogus >/dev/null 2>&1 && fail "bad argument accepted"
echo "ci-changes self-test OK"
