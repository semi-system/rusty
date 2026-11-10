# Docker images and the GitHub workflows

This page describes how the Rusty container image is built, published, signed and verified, how to
build it locally, what the GitHub repository needs configured, and, honestly, which parts could not
be checked on the machine where they were written.

The repository is `semi-system/semi-mail` on GitHub, but no workflow names it: everything derives
from `github.repository` (lower-cased for the registry). The crates and binaries keep the code name
`rusty`. A repository rename needs no workflow change (only the team name in `.github/CODEOWNERS`).

## The image

`deploy/docker/Dockerfile` is a multi-stage build:

* **Compile stage** (`rust:1.99.1-alpine3.22`, pinned by digest): always runs on the build platform and
  cross-compiles to `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`, so the arm64 image is
  not compiled under QEMU. The only C code (ring, bundled SQLite) is compiled with zig 0.15.2 acting
  as the C compiler and linker for the musl target (tarballs pinned by SHA-256 in the Dockerfile).
  Dependencies are resolved with `--locked`; `SOURCE_DATE_EPOCH` (the commit time) and
  `--remap-path-prefix` make the build reproducible as far as the toolchain allows.
* **Final stage** `gcr.io/distroless/static-debian13:nonroot` (pinned by digest): CA certificates, no
  shell, no package manager. The process runs as numeric user `65532:65532`. State lives in two
  volumes: `/var/lib/rusty` (database, blobs, DKIM keys; mode 0700) and `/etc/rusty` (configuration and
  certificates). The root filesystem can be read-only.
* `ENTRYPOINT ["/usr/local/bin/rusty"]`, default command `d --config /etc/rusty/rusty.toml`. The
  binary is multi-call: `rustyd` and `rustyctl` are symlinks next to it, so
  `docker run --entrypoint /usr/local/bin/rustyctl IMAGE ...` works.
* `HEALTHCHECK` runs `rusty healthcheck` (exec form, no shell). It reads the same configuration as the
  daemon and probes each enabled listener on loopback (greeting line on 25/587/143, connect on
  465/993). Exit code 0 healthy, 1 not.
* Binding ports below 1024 as a non-root user needs `cap_add: [NET_BIND_SERVICE]` or the sysctl
  `net.ipv4.ip_unprivileged_port_start=0`.
* OCI labels: `source`, `revision`, `version`, `created` (build arguments `SOURCE_URL`, `REVISION`,
  `VERSION`, `CREATED`).
* Build arg `DEV_CERT=1` keeps the self-signed development certificate feature. That variant exists
  only so `tools/acceptance/local-run.sh` can run against an image. It is never published.

`scripts/check-docker-pins.sh` (a stage of `scripts/ci.sh`) fails when a Dockerfile base image is not
pinned by a digest listed in `deploy/images.lock`, when the Rust tag differs from
`rust-toolchain.toml`, when the zig checksum is missing, and when a workflow action is not pinned by
a full commit SHA with a version comment (or lacks top-level `permissions: {}`, or uses
`pull_request_target`).

### Build and check locally

```sh
docker build -f deploy/docker/Dockerfile -t rusty:local \
  --build-arg VERSION=0.1.0 --build-arg REVISION="$(git rev-parse HEAD)" \
  --build-arg SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)" .
bash scripts/check-image.sh rusty:local          # user, no shell, static, healthcheck, labels, fail-closed start
# the dev-cert variant, run read-only with all capabilities dropped, then the mail scenario against it:
docker build -f deploy/docker/Dockerfile --build-arg DEV_CERT=1 -t rusty:dev .
bash scripts/check-image.sh rusty:dev dev
IMAGE=rusty:dev bash tools/acceptance/local-run.sh
# multi-arch (needs buildx; the arm64 compile is a cross compile, no emulation):
docker buildx build --platform linux/amd64,linux/arm64 -f deploy/docker/Dockerfile .
```

## Workflows (`.github/workflows/`)

Security rules, enforced by `scripts/check-docker-pins.sh` and `actionlint`:

* every third-party action is pinned by its full commit SHA with the version in a comment; Dependabot
  (`github-actions` ecosystem, weekly) proposes updates, including for `abxst/actions`;
* top-level `permissions: {}`, and each job asks only for what it needs (`contents: read`;
  `packages: write` only on the publish and sign jobs; `id-token: write` and `attestations: write` only
  on the sign job);
* no `pull_request_target`; pull requests only build (`push: false`) and receive no secrets (none are
  used at all, only the job-scoped `GITHUB_TOKEN`); `persist-credentials: false` on every checkout;
  untrusted values (ref names) reach scripts through `env`, not through `${{ }}` in the script text;
* concurrency groups per ref (pull request runs are cancelled when superseded).

| Workflow | Trigger | What it does |
|---|---|---|
| `ci.yml` | PR, push to main, nightly, manual | Mirrors `scripts/ci.sh` in jobs: actionlint/shellcheck, gate scripts (`scripts/ci-gates.sh`), clippy and tests on glibc with a Postgres 18 service container (same digest as `deploy/images.lock`) plus the dev quickstart, cargo-deny and cargo-audit, musl tests/release build/symbol gates/static check, and on schedule the fuzz smoke (60 s per target) and the edge suite. `scripts/tests/ci-trap.sh` (a test of `ci.sh` itself) is not run in CI. |
| `docker.yml` | PR, push to main, tags `v*`, manual | `meta` (version, commit time, lower-cased image name) -> `smoke` (builds the image, `check-image.sh`, builds the dev-cert variant and runs `local-run.sh` against it) -> one publish job -> `sign` and `trivy`. |
| `release.yml` | tags `v*` | Builds static `rusty` binaries for amd64 and arm64 with the same Dockerfile (`--target binary`), writes `SHA256SUMS`, signs it keyless (`cosign sign-blob`), attaches the binaries, checksums, `compose.yaml` and (when it exists) the installer, and creates the release with a changelog from `git log`. |

### Publish: the `DOCKER_PUBLISH_MODE` variable

The image build and push is delegated to the maintainer's composite actions, in exactly one of two
mutually exclusive jobs chosen by the repository **variable** `DOCKER_PUBLISH_MODE` (Settings ->
Secrets and variables -> Actions -> Variables; it is not a secret):

| Value | Job | Action |
|---|---|---|
| unset or `no-scan` (default) | `publish-no-scan` | `abxst/actions/no-scan@670736025c229e0c308fc9a488cd5f008a96ca74 # v5`: Buildx, login, metadata, build and push, GHA cache; linux/amd64 and linux/arm64. |
| `strict` | `publish-strict` | `abxst/actions/strict@670736025c229e0c308fc9a488cd5f008a96ca74 # v5`: Semgrep SAST gate, then an amd64 build, a Trivy gate on that image, then push of the scanned image (no rebuild). |

`no-scan` is the temporary default: it is the quicker path while the project is young, but it neither
scans nor signs. `strict` adds the SAST gate and an image-vulnerability gate that blocks the push (we
pass `gate-severity: HIGH,CRITICAL` and `gate-ignore-unfixed: "true"`, matching our own Trivy policy;
the action's defaults would block every severity including unfixed CVEs).

**To switch to strict:** set the variable `DOCKER_PUBLISH_MODE` to `strict` (Settings -> Secrets and
variables -> Actions -> Variables -> New repository variable, or edit it), then re-run the workflow or
push to main. Nothing in the repository changes. **To go back:** delete the variable or set it to
`no-scan`. Run strict once on a branch via "Run workflow" before relying on it, and read the uploaded
`security-reports` artifact.

Neither action signs or attests anything, so the `sign` job always follows the publish job and works
on the pushed **digest**, never on a tag. It first **fails closed** if the digest output is empty or not
`sha256:<64 hex>`. It then:

1. signs the digest with cosign keyless (GitHub OIDC -> Sigstore Fulcio; no key to store);
2. generates an SPDX SBOM with `anchore/sbom-action` and attaches it as a signed cosign attestation
   (`--type spdxjson`);
3. creates a build-provenance attestation (`actions/attest-build-provenance`, pushed to the registry);
4. verifies the signature it just made with the same command given below.

The `trivy` job scans the same digest for HIGH/CRITICAL (fixed vulnerabilities) and uploads
`trivy-report.json`. It is informational (it never fails the run) until the variable
`DOCKER_TRIVY_BLOCKING` is set to `true`; in strict mode the action's own gate is the blocking one.
Exceptions go only in `.trivyignore`, one CVE per line with a reason, reviewer and expiry; it is empty
today. Buildx's own `--provenance/--sbom` are not enabled in the user's actions; our signed attestations
cover the same ground.

Tags pushed: `sha-<short>`, the branch name (`main`), and on `v*` tags `X.Y.Z` and `X.Y`
(`docker/metadata-action` syntax passed through the `tags` input). Pull requests build `pr-<n>` but do
not push. In strict mode the image is amd64 only; see the caveats below.

### Verify an image

Substitute `OWNER/REPO` (the GitHub repository, e.g. the one shown on the package page) in lower
case for the image name and as written for the identity:

```sh
IMAGE=ghcr.io/owner/repo@sha256:<digest>      # always verify and run the DIGEST, not a tag
cosign verify \
  --certificate-identity-regexp '^https://github\.com/OWNER/REPO/\.github/workflows/docker\.yml@' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  "$IMAGE"
cosign verify-attestation --type spdxjson \
  --certificate-identity-regexp '^https://github\.com/OWNER/REPO/\.github/workflows/docker\.yml@' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com "$IMAGE" >/dev/null
gh attestation verify "oci://$IMAGE" --repo OWNER/REPO     # the GitHub provenance attestation
```

Digest pinning for users: copy the digest from a verified release/`main` build into your compose file
as `image: ghcr.io/owner/repo@sha256:...` (a tag can move, a digest cannot). The standalone installer
(planned, after the Postgres backend) pulls by digest and verifies the signature when `cosign` exists.

Release binaries: `cosign verify-blob --bundle SHA256SUMS.cosign.bundle --certificate-identity-regexp
'^https://github\.com/OWNER/REPO/\.github/workflows/release\.yml@' --certificate-oidc-issuer
https://token.actions.githubusercontent.com SHA256SUMS`, then `sha256sum -c SHA256SUMS`.

## What to configure on GitHub (nothing is a secret)

* **Actions -> General -> Workflow permissions:** "Read repository contents and packages permissions"
  (the workflows request more per job where needed). Do not allow Actions to approve pull requests.
  Optionally restrict allowed actions to "Allow actions created by GitHub, plus selected actions"
  and list `docker/*`, `sigstore/*`, `anchore/*`, `aquasecurity/*` and `abxst/actions/*`.
* **Actions -> General -> Fork pull request workflows:** require approval for first-time contributors.
* **Variables:** `DOCKER_PUBLISH_MODE` (`no-scan` or `strict`, default `no-scan`), optional
  `DOCKER_TRIVY_BLOCKING` (`true`).
* **Branch protection on `main`:** require the `ci` jobs, require review from code owners
  (`.github/CODEOWNERS`; replace the team `@semi-system/maintainers` if the organisation or team
  name differs), require signed commits if wanted.
* **Packages:** the first push creates the package `ghcr.io/<owner>/<repo>`; make it public for
  anonymous pulls and connect it to the repository (package settings -> "Manage Actions access"
  must allow this repository with write access, which is automatic when the package is created by
  the workflow).
* **Dependabot:** `.github/dependabot.yml` needs "Dependabot version updates" switched on.
* The GitHub artifact attestations need the repository to be public or on a plan that supports them.
* Tag protection for `v*` so only maintainers can create release tags.

## Known caveats of the maintainer's actions (recommend fixing in `abxst/actions`)

Our repository executes no code from a floating reference: both actions are pinned by commit SHA and
every action we use ourselves is SHA-pinned. But the actions' own dependencies are outside that pin:

* The composite actions pull `docker/setup-buildx-action`, `docker/login-action`,
  `docker/metadata-action`, `docker/build-push-action` (and QEMU in `no-scan`) by major tag (`@v3`,
  `@v5`, `@v6`) inside the `abxst/actions` repository. Recommend pinning those by SHA there.
* `strict` uses `aquasecurity/trivy-action@master` (unpinned, moving) and `semgrep-version: latest`
  (a moving container image). Recommend pinning both.
* `strict` obtains the pushed digest by parsing the `docker push` output with `sed`; if Docker's
  output format changes the digest comes back empty. Our `sign` job fails closed in that case.
* `strict` (v5) builds and scans amd64 only: there is no arm64 image in strict mode. For now we
  accept "amd64 only" in strict mode (the target VPS and VMs are amd64); the Dockerfile stays
  arm64-capable and that path is exercised by `no-scan` mode. `strict` v5 does push the scanned image
  itself, without rebuilding it.
* `no-scan` and `strict` log in to the registry even when `push` is false; with a fork pull request
  the token is read-only, so nothing can be pushed.
* The repository also has other variants (`semi-strict`, `with-scan`); we do not use them.
* The Trivy gate of `strict` does not read our `.trivyignore`.

## Not verified locally (honest list)

The workflows themselves have never run. What was checked on the writing machine:

* `actionlint` (pinned container) is clean on all three workflows, and `shellcheck` on all shell
  scripts, through `scripts/ci-workflows-lint.sh` (also a stage of `scripts/ci.sh`);
  `scripts/check-docker-pins.sh` passes, and both have self-tests.
* The Dockerfile builds with plain `docker build` in WSL; see the verification record in the commit
  message / hand-over for the outputs.

What could not be verified:

* Everything GitHub-side: that the jobs run on hosted runners, the `vars` conditions, the service
  container, the cache steps, the permissions being sufficient (a missing permission shows up as a
  403 on first run), the `needs`/`outputs` wiring between publish, sign and trivy jobs when one of the
  two publish jobs is skipped, GHCR package creation and visibility.
* The real actions' behaviour: the `abxst/actions` inputs were read from their `action.yml` files at the
  pinned SHA, not executed; their tag template output; cosign keyless signing and verification
  (needs a real OIDC token), `anchore/sbom-action`, `attest-build-provenance`, Trivy against a pushed
  image.
* The arm64 image: it is cross compiled, but there is no arm64 machine or emulator here, so the arm64
  binary has not been run (it can only be compiled and its ELF header inspected).
* `release.yml` end to end (the `--target binary` export with `buildx`, `gh release create`).
* The pinned action SHAs were resolved through the GitHub API on the authoring date; they should be
  spot-checked by a human against the release pages before the first run.
* Multi-arch manifest creation, and the nightly fuzz job under real `cargo-fuzz`.
