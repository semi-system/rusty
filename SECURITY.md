# Security
Rusty is a security-first Rust rewrite of the Zimbra mail stack. This file says what the
code defends against today, what it does not, and how to check it.

## Threat model (current state)

Today Rusty is a set of libraries (core, store, directory, MIME); no network services yet.
An attacker who controls any of the following can:

- **Mail content (raw messages):** feed arbitrary bytes to the envelope parser. Only the
  header section (up to the first empty line, at most 256 KiB, else `Invalid`) is parsed,
  so cost is independent of body size, MIME part count and nesting. Output fields carry no
  C0/C1 controls and are capped: subject 2048 bytes, from/to 4096 bytes and 100 whole
  addresses, message ids 998 bytes (else dropped), last 32 references.
- **Addresses / local parts:** submit arbitrary Unicode. Local parts use a strict allow
  list after NFC, lowercase, NFC: ASCII a-z, 0-9, RFC 5322 atext specials and interior
  dots, plus non-ASCII lowercase letters (Ll) in U+00DF..U+00FF (minus U+00F7),
  U+0100..U+024F and U+1E00..U+1EFF. Everything else is rejected (combining marks, other
  scripts, symbols, emoji, spaces, controls, format/private-use/unassigned characters,
  and letters that change under NFKC such as ſ, ĳ, ǆ). Max 64 octets and 64 characters.
  Checked for accounts, aliases, lists and external list members.
- **Blob names and hashes:** hashes must be 64 lowercase hex characters; the blob store
  and GC have tests against hostile names, symlinks and FIFOs (rusty-store tests).
- **Startup config file:** 64 KiB cap, errors never echo contents, a FIFO cannot hang startup.
- **Sizes:** appends over the per-mailbox limit (default 50 MiB) get a quota error;
  `list_folder` and `changes_since` return at most 1000 entries per call.

Errors: messages are fixed short reasons and never echo caller input (`error_leak` test).
The `source()` chain of a storage error may hold raw backend text (SQLite, OS paths) and
must never be serialized to clients.

No `unsafe` in the project crates: `#![forbid(unsafe_code)]` is checked in every
`crates/*/src/lib.rs` and a scan rejects `unsafe` in `crates/*/src` (not `tests/`,
`benches/`, `build.rs`, `fuzz/`). Dependencies do use `unsafe` (SQLite C code, rustix, tokio).

Local-part tradeoff: only Latin is accepted; other scripts and emoji cannot be used yet and
ZWJ/ZWNJ are rejected. Other scripts will be opt-in per tenant under a single-script rule.

## Files and permissions

- Run the service as its own account with **umask 077**; its data directories are 0700.
- The SQLite database file is created 0600 when missing (journal/WAL files inherit it).
- The blob root is created only by `FsBlobStore::init` (0700 plus a `.rusty-blobs` marker,
  0600). `FsBlobStore::open` refuses a missing, unmarked, symlinked or group/world-writable
  root; put and GC never create it. Shard directories are 0700, blob files 0600, written
  via random-named `O_EXCL|O_NOFOLLOW` temp files, renamed, then the directories fsynced.

## Invariants enforced by tests

- `workspace_forbids_unsafe` (rusty-core): forbid attribute in every `lib.rs`, no `unsafe`.
- `local_part_allow_list_*` and the exhaustive allow-list test (rusty-directory).
- Parser bounds and sanitization: `rusty-mime/tests/bounds.rs`, fuzz oracle and proptest.
- Blob store: modes, init/open, symlink/FIFO refusal, and 2,000 put-vs-GC races.
- Schema: `schema_matches_snapshot` pins every table, index and trigger.
- Mailbox size limit: `append_over_limit_is_quota`, `default_limit_is_50_mib`.
- Config: unknown keys, relative paths, bad numbers and wrong-case log levels rejected.
- Address normalization is idempotent (fuzz target and proptest stand-in).

## Not covered yet

- No network services, so no TLS, authentication exposure or protocol parsing.
- No process sandbox (seccomp, namespaces, privilege drop).
- No encryption at rest. Argon2 hashing exists but no rate limiting or lockout yet.
- Homoglyphs: cross-script mixing is impossible, intra-Latin look-alikes remain (`ı`/`i`,
  `ł`/`l`). Follow-up: UTS #39 skeleton uniqueness per tenant.
- `parse_address` trims whitespace around the whole address; protocol layers must reject
  padded input themselves. The Ll test follows the `unicode-general-category` Unicode version.

## Dependency policy

- `deny.toml` (cargo-deny): crates.io only, known vulnerabilities and yanked crates denied,
  unmaintained direct dependencies denied, a license allowlist, no wildcard versions,
  `openssl`/`openssl-sys` banned (TLS will use rustls). The toolchain is pinned in
  `rust-toolchain.toml`.
- `cargo audit` runs in CI. `.cargo/audit.toml` ignores RUSTSEC-2023-0071 (`rsa`): only in
  `Cargo.lock` via the optional sqlx-mysql driver, never built. Re-check on sqlx upgrades.

## Running the checks

    bash scripts/ci.sh

Stages: clippy (`-D warnings`), tests, cargo-deny, cargo-audit, musl tests and release
build, a static-linking check, and 60 s of fuzzing per target (`FUZZ_SECS` overrides).
Without cargo-fuzz/nightly it prints a WARNING and runs only proptest stand-ins.
Fuzz by hand: `cd fuzz && cargo +nightly fuzz run parse_address -- -max_total_time=300`.
Seeds live in `fuzz/seeds/<target>/`.

Tools: `cargo install cargo-deny cargo-audit cargo-fuzz --locked`, `rustup toolchain
install nightly --profile minimal`, `sudo apt install musl-tools`, and
`export CC_x86_64_unknown_linux_musl=musl-gcc` (SQLite is compiled from C).

## Reporting a vulnerability

Do not open a public issue for security problems. Security contact: TODO(maintainer): set contact.
Include the affected commit, a reproducer (for parser bugs, the input bytes), and the
impact you expect. Fixes land with a regression test.
