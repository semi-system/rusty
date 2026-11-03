# Rusty

Rusty is a security-first rewrite of the Zimbra mail stack in Rust.

## Status

This is a library layer. There are no running services yet: no SMTP, IMAP, HTTP or other
network listener, and no daemon. The libraries hold the data model, storage, parsing and
the building blocks for a distributed deployment (idempotent delivery, a job queue,
capabilities). Designs live in `docs/superpowers/specs/`, implementation plans in
`docs/superpowers/plans/`, and the roadmap to feature parity with Zimbra in
`docs/superpowers/plans/2026-11-03-roadmap-zimbra-parity.md`. Several specs (tenants, ACLs,
KMS, edge, containers) describe things that are not implemented.

## Build and test

Development happens in WSL on a native Linux filesystem (not under `/mnt/...`):

```sh
cd ~/Rusty
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The full gate runs clippy, tests (glibc and musl), cargo-deny, cargo-audit, a musl release
build with a static-linking check, a fuzz lockfile check and 60 s of fuzzing per target:

```sh
bash scripts/ci.sh
```

See `SECURITY.md` for the tools it needs. `FUZZ_SECS` overrides the fuzz time.

The toolchain is pinned to an exact version in `rust-toolchain.toml` (rustup installs it on
first use); bump it deliberately as described there. Licensed under MIT (`LICENSE`).

## Acceptance test

`acceptance_10k_messages` drives the whole stack on a file-backed SQLite database and a
filesystem blob store: a domain and account, a three-level folder tree, 10,000 appended
messages (500 exact duplicates, with replies for threading), flag changes, 1,000 moves,
1,000 expunges, purge and blob garbage collection. It checks folder listings and a gapless
change log (both read page by page), blob reference counts and file counts,
`PRAGMA integrity_check` and `PRAGMA foreign_key_check`, and fails if it takes longer than
60 seconds in a debug build.

```sh
cargo test -p rusty-directory --test acceptance -- --nocapture
```

`--nocapture` prints the elapsed time.

## Crates

- `rusty-core`: ids, flags, the shared `Error` type, the startup configuration loader,
  `proto` (protocol version negotiation) and `text` (shared text-hygiene helpers).
- `rusty-store`: SQLite metadata store and the filesystem blob store.
  - `mailbox`: `Mailbox` facade (folders, messages, change log), idempotent `deliver` with
    delivery ids, and batch operations (`move_many`, `expunge_many`, `deliver_many`; one
    transaction each, at most 1000 items).
  - `jobs`: durable job queue with leases and fencing epochs (at-least-once).
  - `blob`, `gc`: content-addressed blob store and garbage collection.
  - `sqlite`, `traits`: schema, connection setup and storage traits.
- `rusty-mime`: bounded parsing of message envelopes (header section only, at most 256 KiB;
  sanitized, capped subject, addresses and threading headers).
- `rusty-directory`: domains, accounts, aliases, lists (expansion capped at 10,000
  members), address resolution, local authentication (`auth`) and hashed, revocable,
  short-lived capability tokens (`caps`).

## Configuration

Startup settings live in a small TOML file; see `rusty.example.toml` for every key and its
default. Unknown keys, relative paths and out-of-range limits are rejected at load time.

Run the service under its own account with umask 077. The blob directory is prepared once
with `FsBlobStore::init` (mode 0700 and a `.rusty-blobs` marker); `FsBlobStore::open`
refuses a missing, unmarked, symlinked or group/world-writable directory instead of
creating an empty store. A missing SQLite database file is created with mode 0600.

## Security posture

Summary (the authoritative text is section 8b of
`docs/superpowers/specs/2026-11-02-rusty-foundation-design.md`):

- No unsafe in project code: each crate declares `#![forbid(unsafe_code)]`.
- All external input is hostile: parsers must not panic and carry size, depth and count limits.
- Default resource limits (message size, header section size, password length, folder
  depth, keywords per message, page size of folder listings and change-log reads).
- Fail closed: errors refuse the operation; error messages never echo caller input, and
  `source()` chains (raw backend text) are never sent to clients.
- Supply chain: `Cargo.lock` is committed; few, well-maintained dependencies.
- Defence in depth: database constraints and triggers back up application checks; no secrets
  in clear text; logs and error messages never contain passwords, hashes or message content.
- Blob garbage collection enforces a minimum grace period and never deletes referenced blobs.
