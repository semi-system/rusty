# Git workflow

Trunk-based: `main` is always releasable and is the only long-lived branch. Everything else is a
short-lived branch merged by pull request. Only the maintainer pushes and merges; agents and
contributors prepare branches and run the gates (the handoff plan says "never push").

## Branches and commits

* Branch names: `type/short-name`, with `type` one of `feat fix perf refactor test docs chore ci build`
  (for example `feat/imap-move`, `fix/smtp-bdat-limit`, `ci/cache-keys`). Keep them short-lived (days, not
  weeks); rebase on `main` instead of merging `main` into the branch.
* Commits follow Conventional Commits, as the existing history does: `type(scope): subject`, imperative,
  no trailing period, scope = crate or area (`feat(imap): ...`, `fix(store): ...`, `docs(plan): ...`).
  Small commits; the body says why. Breaking changes: `type(scope)!:` and a `BREAKING CHANGE:` paragraph.
* No `Co-Authored-By` and no tool-attribution lines. Author and committer must be the identities in
  `scripts/prepush-identity.txt`; `bash scripts/prepush-check.sh` checks this and more (below).
* Signed commits are recommended (SSH or GPG signing), not required.

## Pull requests

* Every change reaches `main` through a pull request. Fill in the template checklist
  (`.github/pull_request_template.md`); it mirrors the repository gates and the security floor.
* **Squash-merge is the default**: one commit per PR on `main`, subject = PR title (Conventional Commits).
  History stays linear (the ruleset also allows rebase-merge, never a merge commit).
* The required status check is **`ci-ok`** (see `docs/ci.md`). It is green when every CI job passed or was
  legitimately skipped (docs-only change).
* No force-push to `main`, no deletion of `main`, no direct pushes. Force-pushing your own feature branch is
  fine until review starts.
* CODEOWNERS review (`.github/CODEOWNERS`) is required for security-sensitive paths: `crates/rusty-smtp`,
  `crates/rusty-imap`, `crates/rusty-svc` (auth and TLS), database migrations (in `crates/rusty-store`),
  `deploy/`, `scripts/`, `.github/`, `Cargo.lock`, `deny.toml`, `SECURITY.md`. CODEOWNERS currently lists
  the team `@semi-system/maintainers` for everything; narrow it per path when more owners exist.
  Parser, auth, concurrency and tenant-isolation changes need an independent review (see the handoff plan).

## Release flow

1. `main` is green and contains everything for the release; bump versions and notes in a normal PR.
2. Tag the merge commit: `git tag -s vX.Y.Z` (or `-a`); the maintainer pushes the tag.
3. The tag triggers `release.yml` (static musl binaries for amd64 and arm64, `SHA256SUMS`, cosign
   signature, GitHub release with notes from the git log) and `docker.yml` (image build, push by digest,
   signature, SBOM, provenance). Tags `v*` are protected by `.github/rulesets/tags.json`.
4. Verify with the commands in `docs/docker.md`.

## Hotfix flow

Fix on `main` first (`fix/...` branch, PR, squash). If a released line must be patched, branch
`release/X.Y` from the tag, cherry-pick the fix (`git cherry-pick -x`), tag the next patch version on that
branch. There is no long-lived development branch to keep in sync. For a security fix follow `SECURITY.md`
(private advisory first, public PR after the fix is ready).

## Bumping toolchain and pins

* GitHub Actions, Dockerfile base images and Cargo dependencies: Dependabot (`.github/dependabot.yml`,
  weekly) opens PRs; nothing auto-merges. Review each: actions stay pinned by full commit SHA with a version
  comment (`scripts/check-docker-pins.sh` fails otherwise). Check the new SHA against the release page.
* Rust toolchain: change `channel` in `rust-toolchain.toml`, the `rust:` tag and digest in
  `deploy/docker/Dockerfile` and `deploy/images.lock` together, run `bash scripts/ci.sh`, fix new lints in
  the same PR.
* CI tools (cargo-deny, cargo-audit, cargo-fuzz): `scripts/ci-tools.txt`. Lint container images:
  `deploy/images.lock`.

## Running the full gate

Same gate everywhere: `bash scripts/ci.sh`.

* Linux, or inside the build server: run it directly.
* Windows with the remote build server (preferred; `docs/buildserver.md`):
  `bash scripts/ci-run.sh --detach`, then `bash scripts/remote-ci.sh --status|--tail|--wait`; single
  cargo commands with `bash scripts/remote-cargo.sh ...`, any other command with
  `bash scripts/remote-run.sh <cmd...>`. Only one full CI at a time on the server.
* Windows with WSL: `bash scripts/wsl-ci.sh`, `bash scripts/wsl-cargo.sh ...`.
* One worktree per task: `git worktree add -b type/name <repo> main`.

## Before every push

```sh
bash scripts/prepush-check.sh                    # range = commits not on origin/*
bash scripts/prepush-check.sh origin/main..HEAD
```

It prints PASS/FAIL per check and exits 1 on any FAIL: author/committer identity against
`scripts/prepush-identity.txt`, `Co-Authored-By` or "Generated with" in messages, blobs over 5 MiB,
private-key material, AWS/GitHub tokens, `password = "..."` literals, workstation paths (`C:\Users`,
`D:\code`, `/home/`) in added lines, files named like keys/env/known_hosts (committed, staged or untracked),
a missing `origin`, and `refs/original` left by a history rewrite. Exceptions are explicit paths in
`scripts/prepush-allow.txt` (reviewed through CODEOWNERS), never patterns. Findings print kind, commit and
`path:line`, never the secret. Note: it scans the whole pushed history, so a leak removed by a later
commit is still reported.

## One-time GitHub setup (repository owner)

1. Create the repository `semi-system/rusty` (empty), run the pre-push check, then `git push -u origin main`.
2. Settings, Rules, Rulesets, New ruleset, Import a ruleset: import `.github/rulesets/main.json`, then
   `.github/rulesets/tags.json`. `main.json` requires a pull request (0 approvals so a sole maintainer can
   merge; raise `required_approving_review_count` to 1 with two maintainers; code owner review stays on),
   the `ci-ok` check with up-to-date branches, linear history, and blocks force-push and deletion.
3. Settings, Actions, General: allow actions from GitHub and the third parties used; workflow permissions
   "read repository contents" (jobs request more themselves); do not let Actions approve pull requests;
   require approval for first-time contributors' workflows.
4. Settings, Secrets and variables, Actions, Variables: `DOCKER_PUBLISH_MODE` = `no-scan` (default) or
   `strict`; optional `DOCKER_TRIVY_BLOCKING` = `true`. No secrets are used.
5. After the first image push: Packages, set the visibility of the `rusty` package and link it to the repository.
6. Optionally add `docker-ok` to the required checks once `docker.yml` has run once.
