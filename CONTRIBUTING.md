# Contributing

Rusty is security-first, early-stage software. Read [SECURITY.md](SECURITY.md) first; report
vulnerabilities privately as described there, never in a public issue or PR.

* Workflow, branches, commits, PRs, release and the pre-push check: [docs/git-workflow.md](docs/git-workflow.md).
* CI and what each job covers: [docs/ci.md](docs/ci.md).
* Where the project stands: [docs/roadmap.md](docs/roadmap.md); how it is built: [docs/architecture.md](docs/architecture.md).
* Working rules: tests first (a test that fails before the change); every CI gate passes, no exceptions; no change that breaks the architecture rules in `docs/architecture.md`; every service fails closed;
  all text in the repository is English, and user-visible strings go through the message catalogues.
* Build and test: `bash scripts/ci.sh` (see [README.md](README.md) and [docs/buildserver.md](docs/buildserver.md)).

Short version: branch `type/short-name`, Conventional Commit messages, tests first, run
`bash scripts/ci.sh` and `bash scripts/prepush-check.sh`, open a PR into `main`; it is squash-merged once
`ci-ok` is green and the code owners reviewed any security-sensitive path.
