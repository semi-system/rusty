# Rusty edge H1 skeleton

This directory contains the HAProxy option-A skeleton and disposable Rust fake
backends. H1 validates static configuration and deployment hardening only. The H2
behavior suite is the required go/no-go gate before production use.

## Run locally

Prerequisites: WSL/Linux, Docker Compose, OpenSSL, and the repository's pinned
Rust 1.99.1 toolchain.

```sh
bash scripts/gen-test-certs.sh
cargo run -p compose-check -- deploy/compose/compose.yaml
docker compose -f deploy/compose/compose.yaml config --quiet
docker compose -f deploy/compose/compose.yaml up --build --wait
docker compose -f deploy/compose/compose.yaml down -v
```

Generated keys live only under the ignored `deploy/compose/generated/` directory.
The generator uses mode 0700 for that directory and 0400 for key-bearing files.
The H1 Compose file runs containers as UID/GID 1000 so a normal WSL developer owns
and can mount those files. Change ownership and the non-root Compose UID together on
hosts using a different developer UID; do not relax key permissions.

Rootless Docker is preferred. Only `edge` publishes ports. The container listens on
high internal ports, so it needs no Linux capability; Docker performs the host-port
mapping. A host may still reserve or restrict public ports below 1024.

## Boundaries and security posture

- `compose-check` rejects non-edge published ports, runtime sockets, privileged or
  host networking, root users, writable root filesystems, missing capability drops,
  missing `no-new-privileges`, missing resource/health limits, unpinned external
  images, and a published HAProxy stats port.
- `compose-check` uses `serde_yaml_ng`, the maintained fork of the retired
  `serde_yaml` crate, and resolves Compose YAML merge keys before applying policy.
- The fake backend image is built from a digest-pinned Rust builder into `scratch`.
  It implements only deterministic protocol shapes for H2; it is not a mail server.
- The test CA and certificates expire after seven days. There is no ACME or `certd`
  integration in H1.
- The runtime/admin socket and stats listener are intentionally absent. H2 may enable
  a mode-0600 Unix socket on private tmpfs solely to test certificate rotation.
- On implicit TLS ports 465, 993 and 995, HAProxy holds tenant private keys in memory
  and sees plaintext before re-encrypting with verified mTLS. This is the accepted
  security trade-off of option A. STARTTLS ports remain byte pass-through.

Replace each `be-*` fake service with the corresponding Rusty service only after H2
passes. Preserve its non-root/read-only/capability/resource constraints and the
verified mTLS plus PROXY-v2 backend contract.

## Behaviour suite and decision (H2/H3)

```sh
bash scripts/wsl-cargo.sh test -p edge-tests -- --ignored --nocapture   # ~19 min, needs Docker
EDGE_ONLY=sni,tls_versions bash scripts/wsl-cargo.sh test -p edge-tests -- --ignored --nocapture
EDGE_SUITE=1 bash scripts/wsl-ci.sh                                      # as a CI stage
```

The suite generates fresh test certificates, starts the project `rusty-edge-h2`, runs every
case, and always tears the project down (`EDGE_KEEP=1` keeps it). Results and the go/no-go
recommendation are in `EDGE-DECISION.md`; the per-behaviour outcomes are in `VERIFIED.md`.
`scripts/ci.sh` always runs `compose-check` on the real compose file and prints a loud NOTE
when the suite is skipped.

What is not production-ready: test certificates only (7 days, CA key beside them), no ACME or
`certd`, keys arrive by bind mount (not tmpfs), fake backends instead of Rusty services, a fixed
host name list in `haproxy.cfg`/`crt-list.txt`. Real services replace the `be-*` fakes by
keeping the contract the fakes enforce: PROXY v2 first, then (for 465/993/995 and HTTP) a TLS
server that requires the edge's client certificate and presents a certificate whose SAN names
the service; STARTTLS ports do their own TLS with their own certificate.

`compose-check` rules (all fail the check with a rule id): `EDGE_ONLY_PORTS`,
`NO_RUNTIME_SOCKET`, `NO_PRIVILEGED`, `NO_HOST_NETWORK`, `NON_ROOT_USER`, `READ_ONLY_ROOTFS`,
`CAP_DROP_ALL`, `NO_NEW_PRIVILEGES`, `RESOURCE_LIMITS`, `IMAGE_PINNED`, `NO_STATS_PORT`.

**Trade-off of option A, stated plainly:** on 465, 993, 995 and 443 the HAProxy process holds
the private keys in memory and sees plaintext before re-encrypting to the backend. A memory
disclosure or code execution bug in HAProxy therefore exposes keys and traffic of those ports.
Ports 25, 587, 143 and 110 are raw pass-through (the edge sees ciphertext after STARTTLS).
