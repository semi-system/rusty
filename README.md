# Rusty Mail Server

A security-first, from-scratch rewrite of the Zimbra FOSS mail stack in Rust: standard
protocols (SMTP, IMAP; JMAP, CalDAV and CardDAV are planned), multi-tenant by design
(domain = tenant), content-addressed blob storage, and an import tool for Zimbra later.
[![ci](https://github.com/semi-system/rusty/actions/workflows/ci.yml/badge.svg)](https://github.com/semi-system/rusty/actions/workflows/ci.yml)

Repository: <https://github.com/semi-system/rusty> (the slug of `repository` in `Cargo.toml`). The crates and binaries keep the code name
`rusty`.

## Status: early development, not production ready

What runs today (single process, `rusty d`):

- SMTP inbound (port 25) and submission (587 STARTTLS / 465 implicit TLS, AUTH only over TLS).
- IMAP (143 STARTTLS / 993 implicit TLS) with IDLE, UIDPLUS, NAMESPACE, SPECIAL-USE and more.
- `rustyctl` administration (domains, accounts, aliases, DNS record hints, DKIM keys).
- Outbound delivery with retries, bounces and DKIM signing.
- PostgreSQL as the only database (repository traits, a migrator role and a runtime role); message bodies
  are files on a volume (an S3-compatible blob store is the planned replacement, see the plans).

Not done yet (see the plans): the admin web UI with DNS setup and validation, packaging, backup and
restore, anti-spam/anti-virus, webmail/JMAP, calendar and contacts. Do not put real mail in a
development instance.

Designs live in `docs/superpowers/specs/`, plans in `docs/superpowers/plans/` (index:
[docs/README.md](docs/README.md)); the current work plan is
`docs/superpowers/plans/2026-11-08-usable-mailserver.md`.

## Try it locally

```sh
bash tools/acceptance/local-run.sh      # needs Docker: starts a throwaway Postgres and a dev instance, sends and reads mail
```

Details, ports and Thunderbird settings: [docs/local-run.md](docs/local-run.md).

## Build and test

Linux (or WSL on a native Linux filesystem):

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/ci.sh        # the full gate (see below)
```

The toolchain is pinned in `rust-toolchain.toml`. PostgreSQL is the only backend, so every test needs a
server: `bash scripts/pg-up.sh` starts a throw-away Postgres 18 container and writes its URL to
`~/.rusty-pg-test-url.dev`; export it as `RUSTY_TEST_PG_URL` before `cargo test` (a missing URL fails the
tests, it does not skip them) and stop it with `bash scripts/pg-down.sh`. The full gate (`scripts/ci.sh`)
starts its own server, and runs clippy, tests (glibc and musl), `cargo-deny`, `cargo-audit`, the repository
policy checks in `scripts/check-*.sh`, a musl release build with a static-linking check, and fuzz smoke runs
for every parser. `FUZZ_SECS` overrides the fuzz time. The tools it needs are listed in
[SECURITY.md](SECURITY.md).

Contributing, branches and PRs: [CONTRIBUTING.md](CONTRIBUTING.md), [docs/git-workflow.md](docs/git-workflow.md); CI: [docs/ci.md](docs/ci.md); run `bash scripts/prepush-check.sh` before `git push`.

Development helpers: on Windows, build in WSL with `scripts/wsl-cargo.sh` / `scripts/wsl-ci.sh`, or on
a dedicated Ubuntu build server with `scripts/remote-cargo.sh` / `scripts/ci-run.sh`
([docs/buildserver.md](docs/buildserver.md)). Files use LF line endings (`.gitattributes`).

## Crates

- `rusty-core`: ids, flags, the shared `Error` type, configuration, protocol version
  negotiation, text hygiene, and the pure DNS record generator.
- `rusty-store`: storage traits and the Postgres backend: `Mailbox` and `Directory`
  repositories, idempotent delivery, batch operations, the durable job queue (leases, fencing),
  content-addressed blob store and garbage collection.
- `rusty-mime`: bounded message parsing (envelope, structure, sections).
- `rusty-directory`: domains, accounts, aliases, lists, address resolution, authentication,
  revocable capability tokens.
- `rusty-svc`: service plumbing (TLS provider, listeners, throttling, memory budget, graceful shutdown).
- `rusty-smtp`, `rusty-imap`: protocol servers (codec, sans-I/O session, driver; third-party
  parsers only inside an adapter module).
- `rusty-ctl`: the `rustyctl` command layer.
- `rusty-runtime`: what every service process shares: the `[store]` section to a store configuration, and the
  signal and exit-code lifecycle (the base of the per-service binaries).
- `rusty`: the composition root and multi-call binary (`rusty d`, `rusty ctl`).
- `tools/`: acceptance scripts, the edge (HAProxy) behaviour suite and its fake backends,
  and the compose hardening checker.

## Configuration

Startup settings live in a TOML file; see `rusty.example.toml` for every key and default. Unknown
keys, relative paths and out-of-range limits are rejected at load time. Run the service under its
own account with umask 077; key and data directories are checked for ownership and modes and the
service refuses to start otherwise.

## Security

No `unsafe` in project code (`#![forbid(unsafe_code)]`), hostile input everywhere (bounded parsers,
fuzzed), fail closed, no secrets in logs or error messages, tenant isolation enforced in the
database as well as in code, and a locked, audited dependency set. The threat model, invariants and
how to report a vulnerability are in [SECURITY.md](SECURITY.md).

## License

MIT, see [LICENSE](LICENSE).
