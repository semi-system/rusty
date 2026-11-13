<!-- Title: Conventional Commits, e.g. `feat(imap): add MOVE`. The PR is squash-merged and the title becomes the commit subject. -->

## What and why

## Checklist

- [ ] `bash scripts/ci.sh` is green (locally, on the build server or in WSL), or CI is green and the reason a local run was skipped is stated here
- [ ] Tests were written first and failed for the stated reason before the change; security and concurrency tests were shown to catch a mutation
- [ ] Every database behaviour test runs on both backends (SQLite and Postgres) or carries a reasoned backend-neutral marker (`scripts/check-parametrised.sh`)
- [ ] A new SQLite migration has its Postgres twin with the same number and regenerated snapshots; no Postgres number 0002..0009 (`scripts/check-pg-migration-gap.sh`)
- [ ] Authentication is accepted only over TLS; no change widens what an unauthenticated peer can reach
- [ ] No secrets, passwords, tokens, key material or mail content in logs, `Debug`, errors, fixtures or this PR
- [ ] Parsers and protocol code keep their limits, never panic on input and fail closed
- [ ] Service crates reach storage only through the repository facades; no raw pool or SQL outside `rusty-store`
- [ ] New dependency: licence checked (no AGPL, no OpenSSL), `cargo deny check` and `cargo audit` green, no new ignore without a written reason
- [ ] Workflow or script change: actionlint and shellcheck clean (`bash scripts/ci-workflows-lint.sh`); every action pinned by full commit SHA with a version comment
- [ ] Docs updated (`docs/`, `README.md`, the plan the work belongs to)
- [ ] No `Co-Authored-By` or tool-attribution lines in the commits; `bash scripts/prepush-check.sh` passes

## Security-sensitive paths

<!-- CODEOWNERS review is required for crates/rusty-smtp, crates/rusty-imap, crates/rusty-svc (auth and TLS), migrations, deploy/ and .github/. Say here which of them this PR touches and what you tried to break. -->
