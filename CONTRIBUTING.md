# Contributing

Rusty is security-first, early-stage software. Read [SECURITY.md](SECURITY.md) first; report
vulnerabilities privately as described there, never in a public issue or PR.

* Workflow, branches, commits, PRs, release and the pre-push check: [docs/git-workflow.md](docs/git-workflow.md).
* CI and what each job covers: [docs/ci.md](docs/ci.md).
* Where the project stands and the working rules (TDD, gates, architecture invariants, security floor):
  [docs/superpowers/plans/2026-11-10-handoff-remaining-work.md](docs/superpowers/plans/2026-11-10-handoff-remaining-work.md)
  and the master plan [docs/superpowers/plans/2026-11-08-usable-mailserver.md](docs/superpowers/plans/2026-11-08-usable-mailserver.md).
* Build and test: `bash scripts/ci.sh` (see [README.md](README.md) and [docs/buildserver.md](docs/buildserver.md)).

Short version: branch `type/short-name`, Conventional Commit messages, tests first, run
`bash scripts/ci.sh` and `bash scripts/prepush-check.sh`, open a PR into `main`; it is squash-merged once
`ci-ok` is green and the code owners reviewed any security-sensitive path.
