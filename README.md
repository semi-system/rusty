# Rusty

Rusty is a security-first rewrite of the Zimbra mail stack in Rust. This repository holds
the foundation layer: shared types, a SQLite metadata store with a content-addressed blob
store, a MIME envelope parser, and a directory of domains, accounts, aliases and lists with
password authentication. Network services (SMTP, IMAP, HTTP) are not part of this layer.

## Build and test

Development happens in WSL on a native Linux filesystem (not under `/mnt/...`):

```sh
cd ~/Rusty
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The toolchain channel is set in `rust-toolchain.toml`.

## Acceptance test

`acceptance_10k_messages` drives the whole stack on a file-backed SQLite database and a
filesystem blob store: a domain and account, a three-level folder tree, 10,000 appended
messages (500 exact duplicates, with replies for threading), flag changes, 1,000 moves,
1,000 expunges, purge and blob garbage collection. It checks folder listings, a gapless
change log, blob reference counts and file counts, `PRAGMA integrity_check` and
`PRAGMA foreign_key_check`, and fails if it takes longer than 60 seconds in a debug build.

```sh
cargo test -p rusty-directory --test acceptance -- --nocapture
```

`--nocapture` prints the elapsed time.

## Crates

- `rusty-core`: ids, flags, the shared `Error` type and the startup configuration loader.
- `rusty-store`: SQLite metadata store, `Mailbox` facade, filesystem blob store, garbage collection.
- `rusty-mime`: bounded parsing of message envelopes (subject, addresses, threading headers).
- `rusty-directory`: domains, accounts, aliases, lists, address resolution, local authentication.

## Configuration

Startup settings live in a small TOML file; see `rusty.example.toml` for every key and its
default. Unknown keys, relative paths and out-of-range limits are rejected at load time.

## Security posture

Summary (the authoritative text is section 8b of
`docs/superpowers/specs/2026-11-02-rusty-foundation-design.md`):

- No unsafe in project code: each crate declares `#![forbid(unsafe_code)]`.
- All external input is hostile: parsers must not panic and carry size, depth and count limits.
- Default resource limits (message size, password length, folder depth, keywords per message).
- Fail closed: errors refuse the operation.
- Supply chain: `Cargo.lock` is committed; few, well-maintained dependencies.
- Defence in depth: database constraints and triggers back up application checks; no secrets
  in clear text; logs and error messages never contain passwords, hashes or message content.
- Blob garbage collection enforces a minimum grace period and never deletes referenced blobs.
