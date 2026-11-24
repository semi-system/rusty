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
  not compiled under QEMU. The only C code (ring) is compiled with zig 0.15.2 acting
  as the C compiler and linker for the musl target (tarballs pinned by SHA-256 in the Dockerfile).
  Dependencies are resolved with `--locked`; `SOURCE_DATE_EPOCH` (the commit time) and
  `--remap-path-prefix` make the build reproducible as far as the toolchain allows.
* **Final stage** `ghcr.io/semi-system/baseless-static` (the baseless base, pinned by digest in `deploy/images.lock`; linux/amd64 only for now): CA certificates, tzdata, no
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

## One image per service

`docker build --target rusty-smtp-in|rusty-submission|rusty-imap|rusty-outbound|rusty-gc` builds one image per service from the same build stage: one static binary
(`/usr/local/bin/rusty-<service>`) and nothing else, no shell, numeric user `65532`, and a configuration file of its own, `/etc/rusty/<service>.toml`, that accepts only the sections
that service uses (`StartupConfig::load_for(role, ...)`; a section of another service is refused at start). The admin web has its own image (`--target admin`) and the all-in-one
`rusty` image (the default target) stays until the split deployment replaces the dev stack.

| Image | Ports | What it runs | Background services |
|---|---|---|---|
| `rusty-smtp-in` | 25 | unauthenticated inbound mail | none |
| `rusty-submission` | 587, 465 | authenticated submission, enqueues outbound jobs | none |
| `rusty-imap` | 143, 993 | IMAP | the change feed (IDLE) |
| `rusty-outbound` | none | the delivery worker, DKIM, relays, DNS and storage checks | the change feed (wake-up) |
| `rusty-gc` | none | blob and row garbage collection, store upkeep (pool repair) | none |

Each binary takes `--config FILE` (default `/etc/rusty/<service>.toml`) and `--log-format text|json`; `<binary> healthcheck` probes its ports (a service with no port, outbound and gc,
only proves that its configuration loads). Every service that reads or writes message bodies may carry `[blob_s3]` and `[unsealing]`: a tenant's bucket keys are sealed to the
unsealing key, so the services that open a tenant's store need it (decision 2026-11-15). `scripts/check-service-image.sh SERVICE IMAGE` checks an image (user, no shell, ports,
entrypoint, healthcheck, one executable, static, fails closed), and the docker workflow builds and checks all five.

Known limits of this step: all five binaries still link the same protocol crates (the split of `rusty-smtp` into four crates and the per-binary dependency gate come next), and the
smtp-in service has no `[outbound]` section, so it runs without the inbound SPF/DKIM/DMARC checks until a `[resolver]` section exists for it.

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
* The amd64 image builds with plain `docker build` in WSL (production and dev-cert variants, about 20 MB):
  `scripts/check-image.sh` passes (numeric user 65532, no shell, statically linked, `ldd` finds nothing,
  healthcheck configured, labels, fail-closed start without a config on a read-only root), the dev-cert
  variant runs read-only with all capabilities dropped and is healthy, and
  `IMAGE=rusty:dev bash tools/acceptance/local-run.sh` passes every check against it. The production
  image refuses `--dev` (no dev-cert feature).

What could not be verified:

* Everything GitHub-side: that the jobs run on hosted runners, the `vars` conditions, the service
  container, the cache steps, the permissions being sufficient (a missing permission shows up as a
  403 on first run), the `needs`/`outputs` wiring between publish, sign and trivy jobs when one of the
  two publish jobs is skipped, GHCR package creation and visibility.
* The real actions' behaviour: the `abxst/actions` inputs were read from their `action.yml` files at the
  pinned SHA, not executed; their tag template output; cosign keyless signing and verification
  (needs a real OIDC token), `anchore/sbom-action`, `attest-build-provenance`, Trivy against a pushed
  image.
* The arm64 image: it is cross compiled (the first attempt failed on a GNU-ld-only linker flag, now filtered
  in the `zigld` wrapper; the re-run of the arm64 compile was interrupted by a WSL failure and is NOT
  confirmed), and there is no arm64 machine or emulator here, so the arm64
  binary has not been run (it can only be compiled and its ELF header inspected).
* `release.yml` end to end (the `--target binary` export with `buildx`, `gh release create`).
* The pinned action SHAs were resolved through the GitHub API on the authoring date; they should be
  spot-checked by a human against the release pages before the first run.
* Multi-arch manifest creation, and the nightly fuzz job under real `cargo-fuzz`.

## Spam and virus filters (the `filter` profile)

`./setup.sh --filter ...` (with the usual arguments) adds a validating resolver (Unbound), rspamd, ClamAV (clamd) and its signature updater (freshclam) to the standalone stack, and writes a `[filter]` section into the inbound SMTP service's configuration. The scanners sit on the `filter` network (10.78.0.0/24), which has no route to the internet and no access to the database; only the resolver and freshclam also join `egress`. Inbound mail is scanned for viruses first (a message is held with 451 while clamd is down), then scored (a message gets through when rspamd is down). Spam goes to the Spam folder, virus mail is refused with 550. No greylisting: `greylist` in `filter/rspamd/actions.conf` is out of reach on purpose. The first start takes a few minutes while clamd waits for its first signatures. Design: docs/design-filters-and-resolver.md.

## One image per service, published to ghcr

The last stage of the Dockerfile is selected by the `SERVICE` build argument (default `rusty`, the multi-call mail image). CI builds one image per stage and publishes each to its own
package, signs it (cosign, keyless), attaches an SBOM and scans it: `ghcr.io/semi-system/rusty` (multi-call, used by the one-shot jobs), `rusty-smtp-in`, `rusty-submission`,
`rusty-imap`, `rusty-outbound`, `rusty-gc` and `rusty-admin`. Tags: `sha-<short>`, the branch name, and `1.2.3` / `1.2` for a release tag.

A server never builds. `deploy/standalone/setup.sh` pulls (`docker compose pull`) the images of the tag in `.env` (`--image-tag 1.2.3`, default `main`; `--registry` for a mirror), and an
upgrade is: back up, set the new tag, `./setup.sh ...` again, `docker compose up -d` (the migration runs first); the whole procedure (or just `./upgrade.sh --image-tag X`), the rollback and the runbooks are in `operations.md`. Verify an image before you run it:

```sh
cosign verify \
  --certificate-identity-regexp '^https://github.com/semi-system/rusty/\.github/workflows/docker\.yml@' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  ghcr.io/semi-system/rusty-smtp-in:1.2.3
```

For development, `./setup.sh --build ...` writes `COMPOSE_FILE=compose.yaml:compose.build.yaml` into `.env`: the same image names, built from the checkout.

## Scaling the services (standalone stack)

The mail services keep no authoritative state of their own (coordination is through PostgreSQL), so more replicas of `smtp-in`, `submission`, `imap`, `outbound`, `gc` and `admin` run side by side:

```sh
docker compose up -d --scale smtp-in=3 --scale imap=2
```

Nothing is addressed by IP. The replicas get whatever address Docker gives them, the edge reaches them by service name through Docker's resolver (a HAProxy `server-template` per service, re-resolved every few
seconds; a replica that does not accept connections is taken out), and the services find each other (`clamd`, `rspamd`, the database) by name. Two containers keep an address on purpose: the edge, because it
owns the host ports (one per host) and the services accept the PROXY header and the admin web accepts connections from it only, and the resolver of the `filter` profile, because rspamd takes its nameserver
as an address.

What a replica needs:

* **The database allows it by name.** `pg_hba.conf` lists `rusty-<service>-1`, `-2`, ... up to `./setup.sh --max-replicas N` (default 4, at most 99); run `setup.sh` again to raise it. A replica beyond the
  number is refused by the database, not silently ignored.
* **Message storage reachable from every replica.** Replicas on one host share the `mail-data` volume; replicas on several hosts need an S3-compatible blob store (`blob-stores.md`).
* **Shared limits need Valkey.** Without `[valkey]` the sending budget per account and the login throttle are counted per process, so the effective limit grows with the replicas (`valkey.md`).
* **Database connections add up.** Every replica opens its own pool (`[store] max_connections`); put PgBouncer in front when the sum nears the server's `max_connections` (`postgres-production.md`).

## Metrics

`./setup.sh --metrics-allow 192.168.1.10/32 ...` (repeatable: one to sixteen networks, never every address) turns the metrics on. Every service then has a `[metrics]` listener on port 9100 (the internal `metrics` network,
`rusty_metrics`, which no port of the host reaches) that only the admin web may read (`allowed_hosts = ["tasks.admin", "admin"]`: the service name, resolved again every few seconds, so neither an address nor a subnet is fixed and a swarm works the same); the admin web is the **central endpoint**: it reads every replica of every service (Docker's
resolver returns them all) and serves everything on `admin:9100`, each sample labelled `service` and `instance`, plus `rusty_scrape_target_up` (0 for a service that cannot be read). Only the networks named by
`--metrics-allow` may read the admin web's endpoint. The stack does **not** run a Prometheus: yours joins the `rusty_metrics` network (`docker run --network rusty_metrics ...`, or an override file) and has ONE
target, `admin:9100` (`deploy/monitoring/prometheus.yml`), so no service can be missed. `deploy/monitoring/rusty.rules.yml` has sample alert rules; `docs/design-metrics.md` lists every metric.
`scripts/check-prom-rules.sh` runs `promtool` on both samples in CI.

## The deployment kit (one file to download)

Every release has `rusty-deploy-TAG.tar.gz`: the `deploy/` directory without the development and build-server parts, `swarm.env.example`, and a launcher `rusty-swarm`. It is listed in `SHA256SUMS`, which is signed with cosign
like the binaries (verify the signature, then the sum; `cosign verify-blob` as above). The setup is two commands and nothing else is taken from the repository:

```
tar xzf rusty-deploy-TAG.tar.gz && cd rusty-deploy-TAG
cp swarm.env.example swarm.env      # the nodes and the database; the file stays yours
./rusty-swarm init                  # Docker where it is missing, the swarm, the database roles
```

The archive is reproducible (sorted names, a fixed time, no owner): the same tree and tag give the same bytes. `scripts/package-deploy.sh TAG [OUTDIR]` makes it by hand; `scripts/tests/package-deploy.sh` is its gate.

**The images of a kit are pinned by digest.** On a release tag `docker.yml` builds and signs the ten service images (`rusty`, `rusty-smtp-in`, `-submission`, `-imap`, `-outbound`, `-gc`, `-admin`, `-webmail`, `-dav`, `-dns`, each as `ghcr.io/OWNER/NAME:X.Y.Z`),
and `release.yml` runs `scripts/pin-release-images.sh`: it waits until each image exists **and** carries its keyless cosign signature (made by `docker.yml` of this repository on that tag), then writes one line per image
(`name:tag  name@sha256:digest`) at the end of the kit's `deploy/images.lock`, after the pins of the third-party images; `deploy/RELEASE` holds the tag. So a kit names the exact bytes built for its release and cannot
pull a newer `main`, and an image that is not signed is never pinned (the release fails instead). `scripts/tests/package-deploy.sh` checks this with a stub registry.

**Without a tag: the release `kit-main`.** `kit.yml` runs after `docker.yml` has built and signed the images of a commit on `main`: it makes `rusty-deploy-main-<sha>.tar.gz` and `.zip` with that commit's images pinned by digest, signs `SHA256SUMS`
with cosign, and puts them on the prerelease `kit-main`, replacing what was there. Download from the Releases page, or `gh release download kit-main`; check the sums (`sha256sum -c SHA256SUMS` after `cosign verify-blob`),
unpack, `./rusty-swarm init`. It always holds the newest `main`; for a fixed version use a tagged release (`release.yml`, the same kit).
