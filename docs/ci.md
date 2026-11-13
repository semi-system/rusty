# CI on GitHub Actions: design, speed, and what is verified

Workflows: `.github/workflows/ci.yml` (this page), `docker.yml` (image, see `docs/docker.md`),
`release.yml` (tag `v*`). The security rules of `docs/docker.md` apply to all of them and are enforced
by `scripts/check-docker-pins.sh` and actionlint: every third-party action pinned by full commit SHA
with a version comment, `permissions: {}` at the top and least privilege per job, no
`pull_request_target`, `persist-credentials: false`, no secrets (only the job-scoped `GITHUB_TOKEN`).

**Branch protection needs one check: `ci-ok`** (importable ruleset: `.github/rulesets/main.json`,
instructions in `docs/git-workflow.md`).

## Job graph of `ci.yml`

```
changes ─┬─ workflows-lint          (actionlint + shellcheck + their self-test)
         ├─ gates                   (scripts/ci-gates.sh static, compose check, fuzz lockfile, pg isolation)
         ├─ test-glibc              (clippy + cargo test --workspace + dev quickstart smoke)   [Postgres service]
         ├─ test-postgres           (scripts/ci-pg-suites.sh)                                  [Postgres service]
         ├─ supply-chain            (cargo-deny, cargo-audit)
         ├─ musl-test               (cargo test --workspace --target x86_64-unknown-linux-musl)
         └─ musl-release            (release build, symbol gates, static-link check)
fuzz-smoke, edge-suite              (schedule / manual only)
ci-ok                               (always runs; needs every job above)
```

All jobs after `changes` run in parallel. Wall time = the slowest job, not the sum.

### The `changes` job

`scripts/ci-changes.sh` (plain shell, no third-party action, self-test `scripts/tests/ci-changes.sh`)
reads `git diff HEAD^1 HEAD` (the merge commit GitHub checks out for a pull request) and prints
`code=`, `rust=`, `docker=`, `workflow=`.

| Output | True when a changed path is... | Jobs gated |
|---|---|---|
| `code` | outside documentation and repository metadata (`docs/**`, `*.md` outside `crates/ tools/ fuzz/ deploy/`, `LICENSE`, issue/PR templates, `CODEOWNERS`, `dependabot.yml`, `.github/rulesets/`) | `workflows-lint`, `gates` |
| `rust` | outside the above and outside the image-only inputs (`deploy/docker/**`, `deploy/images.lock`, `.dockerignore`, `.trivyignore`, `docker.yml`, `release.yml`, the image check scripts) | tests, Postgres, supply chain, musl |
| `docker` (docker.yml) | one of the image build inputs (workspace, `deploy/`, image scripts, `docker.yml`) | image smoke matrix, publish |

Fail-safe rules: only pull requests are filtered. Pushes to main, tags, the nightly schedule and
manual runs always run everything (they seed the caches and are the release gate). An empty or unreadable
diff runs everything. One document is read by a test (`docs/superpowers/plans/2026-11-03-postgres-backend-mutation-table.md`)
and is therefore classified as code. A docs-only pull request skips every heavy job and still ends with
a green `ci-ok`.

### `ci-ok`

Runs with `if: always()`, needs every job, and fails when `changes` did not succeed or any job's result
is anything but `success` or `skipped` (so a failed or cancelled job fails it; a legitimately skipped job,
a docs-only change or the nightly-only jobs, does not). Branch protection requires only this check, so
adding or renaming jobs never needs a settings change. `docker.yml` has the same aggregator, `docker-ok`;
it is optional as a required check (see `docs/git-workflow.md`).

## Caching

| Cache | Key | Restore-keys | Saved |
|---|---|---|---|
| cargo registry and git (`~/.cargo/registry/{index,cache}`, `~/.cargo/git/db`) | `cargo-registry-v1-<os>-<hash Cargo.lock>` | `cargo-registry-v1-<os>-` | by `test-glibc` only, on main, on a miss |
| build directory `target/` (one per job: gates, test-glibc, test-postgres, musl-test, musl-release, edge) | `cargo-target-v1-<job>-<os>-<hash Cargo.lock + rust-toolchain.toml>` | `cargo-target-v1-<job>-<os>-` | on main only, on an exact-key miss, after `scripts/ci-cache-prune.sh` |
| `fuzz/target` (nightly) | `cargo-target-v1-fuzz-<os>-<hash fuzz/Cargo.lock + rust-toolchain.toml>` | prefix | main only |
| `~/.ci-tools` (cargo-deny, cargo-audit; cargo-fuzz) | `ci-tools-v1-<set>-<os>-<hash scripts/ci-tools.txt + toolchain>` | none | main only |

* Pull requests only **restore** (`actions/cache/restore`; GitHub lets a PR read the default branch's
  caches) and never save, so PRs cannot evict main's entries or fill the 10 GiB repository quota.
  The nightly run on main refreshes entries and keeps them from the 7-day idle eviction.
* The registry cache is separate from `target/` so a source-only change reuses the (large, slow) dependency
  build and a new dependency only refetches crates.
* `scripts/ci-cache-prune.sh` runs before saving: it deletes incremental data and every artefact of the
  workspace crates (they change with each commit and are rebuilt anyway) and all test binaries, keeping
  the third-party dependency builds, which are the expensive part. Self-test: `scripts/tests/ci-cache-prune.sh`.
* Tools: cargo-deny 0.20.0, cargo-audit 0.22.0 and cargo-fuzz 0.13.1 are pinned in `scripts/ci-tools.txt`
  and installed by `scripts/ci-tool.sh` with `cargo install --locked --root ~/.ci-tools --version X`; the
  directory is cached so the compile (cargo-deny: more than two minutes) happens only when the pin or the
  toolchain changes. **A prebuilt-binary-with-checksum method was NOT used**: the official release checksums
  are on GitHub, which could not be contacted when this was prepared, and a checksum must not be written
  from memory. If you want it: download the release asset on a trusted machine, record its SHA-256 in
  `scripts/ci-tools.txt`, and have `ci-tool.sh` verify it with `sha256sum -c` before extracting.
  `cargo install --locked` still verifies every crate against the registry checksums. The advisory
  databases are fetched at run time and are never cached (a stale database would hide advisories).
* Not cached: the Rust toolchain itself (a `rustup set profile minimal` step avoids the rust-docs download
  instead), Docker image layers of the Postgres service container.

Bumping the key prefix (`-v1-`) invalidates every cache at once if one is ever poisoned or corrupted.

## Other speed measures

* `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=line-tables-only` for all jobs: less to compile, link and
  cache; panics and test failures still show file and line. Release builds are not affected.
* Clippy and `cargo test --workspace` share one job and one target directory (`test-glibc`), as before.
* The Postgres suites (`scripts/ci-pg-suites.sh`: store with `postgres`, directory+imap, the mutation-table
  test, clippy of those crates with the feature) moved out of `test-glibc` into their own job, which runs in
  parallel. They need the `postgres` feature graph, which the workspace build does not share, so this is the
  only split that costs no extra compile. **They are not sharded further**: every shard would recompile that
  graph, and without a measurement on GitHub runners it is not known to be a win. If the job becomes the
  critical path, shard by command of `ci-pg-suites.sh` (each command is independent; give each shard its own
  service container).
* musl: the debug test build and the release build share no artefacts, so they are two parallel jobs instead
  of one after the other. Coverage is identical to `scripts/ci.sh`: musl tests, musl release build,
  the three symbol gates (`scripts/ci-gates.sh symbols`) and the static-linking check.
* `concurrency` cancels superseded runs of the same pull request and of main. Scheduled and manual runs have
  their own group and are never cancelled by pushes. `timeout-minutes` are kept on every job.
* Nightly only: fuzz smoke and the edge suite.

## `docker.yml`

* `meta` also classifies the diff. On pull requests the image jobs run only when something the image is
  built from changed (workspace, `deploy/`, image scripts, `docker.yml`); pushes and tags always build.
* The smoke job is now a three-way matrix, one runner each, in parallel: amd64 production image
  (`check-image.sh`), amd64 dev-cert variant (`local-run.sh` scenario), arm64 cross-compile (the
  `--target binary` export release.yml uses; checks the ELF is aarch64 and static). Before, the two amd64
  builds ran one after the other and arm64 was only compiled by the publish job.
* On pull requests the publish jobs (`push: false`, multi-arch) run only when `docker.yml` itself changed
  (this keeps testing the `abxst/actions` inputs, e.g. in a Dependabot bump) because the smoke matrix now
  already compiles both architectures. On main and tags they run as before, after the smoke matrix.
* Layer cache: the publish action (`abxst/actions`) owns Buildx and its GitHub-Actions cache. Its inputs and
  whether it uses `type=gha,mode=max` could **not** be inspected here, so this is unverified; check the first
  run's log for `importing cache manifest from gha`. The smoke jobs use the runner's plain Docker daemon with
  no persistent layer cache: giving them `type=gha` needs `docker/setup-buildx-action` and
  `docker/build-push-action` (or the runtime token), which would have to be pinned by commit SHA that could not
  be looked up offline. The zig tarball, `apk` and `rustup target add` layers are therefore re-created in each
  smoke job; the Dockerfile's cargo cache mount does not survive between runners either. Adding the two
  pinned actions with `cache-from/cache-to: type=gha,mode=max,scope=smoke-<variant>` is the next step if the
  smoke jobs turn out to dominate; the cargo compile inside the Dockerfile is the larger cost and is not
  reusable without a cache-mount export action.
* `docker-ok` aggregates the workflow like `ci-ok`.
* `cancel-in-progress` stays pull-request only: cancelling a main run could stop a registry push half way.

## Coverage compared with `scripts/ci.sh`

| `ci.sh` stage | Where it runs on GitHub |
|---|---|
| ci scripts (`tests/ci-trap.sh`, runs `ci.sh` itself) | not in CI (recursive); run it locally |
| remote build helpers self-test, ci-changes / ci-cache-prune / ci-tool / prepush-check self-tests | `gates` (via `ci-gates.sh static`) |
| raw-db, fixtures, mutation hook, pg-migration-gap, wrapped-libs, service-crates, test-knobs, docker pins (gate + self-test) | `gates` |
| workflows-lint self-test; actionlint and shellcheck | `workflows-lint` |
| clippy; test (glibc); dev quickstart smoke | `test-glibc` |
| postgres (store, directory, imap, mutation table, clippy) | `test-postgres` |
| pg isolation | `gates` |
| cargo-deny, cargo-audit | `supply-chain` |
| test (musl) | `musl-test` |
| build (musl release); no test knobs/fixtures/mutation hook in release; static linking | `musl-release` |
| compose hardening check; fuzz lockfile | `gates` |
| edge suite, fuzz smoke | nightly / manual |

Nothing from `ci.sh` was dropped. Differences that already existed and remain: the proptest fallback of the
fuzz stage is replaced by real `cargo-fuzz` on schedule, and `ci-trap.sh` is not run.

## Estimated time (ESTIMATES, not measurements: GitHub could not be run)

Assumptions: `ubuntu-24.04` standard runners (4 vCPU public repositories, 2 vCPU private: roughly double
the numbers), about 300 crates in the lockfile, release-profile musl builds.

| Scenario | Before | After |
|---|---|---|
| Docs-only pull request | everything, about 30 to 45 min | about 1 min (`changes` + `ci-ok`) |
| Code PR, caches warm (main saved them) | about 30 to 40 min: `test-glibc` ran clippy, tests, the Postgres feature build and suites in sequence (about 25 to 35 min); `musl` ran debug tests then release build (about 18 to 25 min); `supply-chain` ran in parallel | about 12 to 18 min: the slowest of `test-glibc` (clippy + tests + quickstart), `test-postgres`, `musl-test`, `musl-release` |
| Code PR, no cache yet | about 45 to 60 min | about 20 to 28 min (the slowest job builds its dependencies once) |
| `supply-chain` only | 5 to 8 min when the tool cache missed (before, PR runs saved the cache and could evict it) | 1 to 2 min warm; one compile of about 4 to 6 min on a tool pin change |
| Docker workflow, image changed (PR) | smoke about 15 to 25 min (two builds in sequence), then multi-arch publish build about 15 to 25 min | one matrix round of about 8 to 14 min (parallel); publish build skipped unless `docker.yml` changed |
| Docker workflow, nothing the image uses changed (PR) | the same as above | seconds (`meta` + `docker-ok`) |

How the numbers were derived: the old workflow serialised the two largest compile chains (workspace tests
and the `postgres` feature graph) in one job and two profiles in the musl job, so its critical path was the
sum of those; the new one runs them side by side, so the critical path is the longest single chain. Dependency
builds dominate a cold run and vanish on a cache hit; the workspace crates themselves (a few minutes) are
always rebuilt. The percentages are reasoning, not data; replace them with the first real run's timings
(job durations in the Actions UI) and correct this page.

### Measured (on the build server, not GitHub)

* Two crates (`rusty-core`, `rusty-mime`), `cargo test --no-run`, empty target dir: 593 MiB with default dev
  debug info, 401 MiB with `CARGO_PROFILE_DEV_DEBUG=line-tables-only` (about one third less to write, link
  and cache); build time 18 s versus 12 s on that subset on a machine that was running other work (noisy).
* `scripts/ci-cache-prune.sh` on that 401 MiB directory: 225 MiB left (the dependency builds).
* Every script the workflows call was run on the build server: `scripts/ci-gates.sh static` (green, about 1 min),
  the lint (`scripts/ci-workflows-lint.sh`, actionlint and shellcheck over 54 shell files: clean), the
  self-tests of the new scripts, `check-docker-pins.sh`. The cargo stages were not re-run (a full CI of main was
  running there); they are unchanged commands.

## What could not be verified here

* That the workflows run on GitHub at all: `actions/cache/restore` and `/save` outputs (`cache-hit`,
  `cache-primary-key`), the `join(needs.*.result, ' ')` aggregator, the `if:` on skipped dependencies, the
  matrix job names, the service container, permissions (a missing one shows as a 403 on the first run).
  actionlint (pinned container) accepts both files.
* Cache sizes against the 10 GiB repository quota: six `target/` caches plus tools. Check the Actions cache
  page after the first main run; drop `edge` or `gates` from saving if the quota is tight.
* Real timings, the pinned tool versions compiling with toolchain 1.99.1 on the runner (they compiled on the
  build server), and `abxst/actions` behaviour.

## Operating notes

* Re-run a flaky run from the Actions UI; `ci-ok` is re-evaluated by the re-run.
* To bump cargo-deny/audit/fuzz: edit `scripts/ci-tools.txt`; the cache key changes and rebuilds them once.
  Keep `deny.toml` compatible with the new cargo-deny.
* To bump the toolchain: `rust-toolchain.toml`, the Dockerfile tag and `deploy/images.lock` together
  (`scripts/check-docker-pins.sh` checks they agree); every target cache rebuilds once.
* Running pieces on the build server: `bash scripts/remote-run.sh bash scripts/ci-gates.sh static`,
  `bash scripts/remote-run.sh bash scripts/ci-workflows-lint.sh` (any command; per-worktree lock, same SSH
  rules as `remote-cargo.sh`; see `docs/buildserver.md`).
