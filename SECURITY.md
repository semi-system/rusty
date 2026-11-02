# Security

Rusty is a security-first Rust rewrite of the Zimbra mail stack. This file says what
the code defends against today, what it does not, and how to check it yourself.

## Threat model (current state)

Today Rusty is a set of libraries (core types, SQLite metadata store, filesystem blob
store, directory, MIME envelope parser). There are no network services yet.

An attacker who controls any of the following can:

- **Mail content (raw messages):** feed arbitrary bytes to the envelope parser. They
  can make it return empty fields; they cannot make it panic, or smuggle `<`, `>` or
  uppercase into parsed addresses.
- **Addresses / local parts / list members:** submit arbitrary Unicode. They cannot
  get invisible or direction-changing characters (zero-width, bidi overrides, BOM,
  private-use, unassigned code points), whitespace, control characters, or over-long
  local parts accepted, so look-alike and "Trojan source" addresses are refused.
- **Filenames / paths / blob hashes:** they cannot escape the blob root; hashes must be
  64 lowercase hex characters, and hostile names in the blob area are quarantined.
- **The startup config file:** it is read with a 64 KiB cap, never echoed in errors,
  and a FIFO or device at the config path cannot hang startup.
- **Message size:** appends over the per-mailbox limit (default 50 MiB) are refused
  with a quota error before any blob is written.

They cannot, in this codebase, run memory-unsafe code: `#![forbid(unsafe_code)]` is on
in every crate and a test fails if an `unsafe` token appears anywhere in `crates/*/src`.

## Invariants enforced by tests

- `workspace_forbids_unsafe` (rusty-core): forbid attribute in every `lib.rs`, no
  `unsafe` token in any source file.
- `rejects_zero_width_and_bidi_override_in_local_part` (rusty-directory): Cf/Co/Cn
  characters rejected for accounts, aliases, lists and list members; accented and CJK
  local parts still accepted.
- Mailbox size limit: `append_over_limit_is_quota`, exact-limit and default (50 MiB).
- Config: unknown keys, relative paths, out-of-range numbers and wrong-case log levels
  are rejected; the 64 KiB + 1 boundary is rejected; FIFOs do not block.
- Blob store and GC: path-escape, hash-format and interrupted-run tests (rusty-store).
- Address normalization is idempotent (fuzz target and proptest stand-in).
- Foundation acceptance: 10,000 messages end to end, with integrity checks.

## Not covered yet

- No network services, so no TLS, authentication exposure or protocol parsing.
- No process sandbox (seccomp, namespaces, privilege drop).
- No encryption at rest for the database or blobs.
- Password hashing exists (Argon2) but there is no rate limiting or lockout yet.
- Unicode categories follow the `unicode-general-category` crate version; characters
  assigned in a newer Unicode release than that crate knows are rejected (fail closed).

## Dependency policy

- `deny.toml` (cargo-deny): crates.io only (no git or unknown registries), known
  vulnerabilities and yanked crates denied, unmaintained crates warned, a license
  allowlist, wildcard version requirements denied, `openssl`/`openssl-sys` banned
  (TLS will use rustls).
- `cargo audit` runs in CI. `.cargo/audit.toml` ignores RUSTSEC-2023-0071 (`rsa`) with
  a written reason: it is only in `Cargo.lock` through an sqlx driver we never build.
  Re-check on every sqlx upgrade.

## Running the checks

    bash scripts/ci.sh

Stages: clippy (`-D warnings`), tests, cargo-deny, cargo-audit, tests and release build
for `x86_64-unknown-linux-musl`, a static-linking check of release binaries, and 60 s
of fuzzing per target (`FUZZ_SECS` overrides). The script stops at the first failure.
If cargo-fuzz or nightly is missing it prints a WARNING and runs the proptest stand-ins
only; that is weaker than fuzzing. To fuzz one target by hand:

    cd fuzz && cargo +nightly fuzz run parse_address -- -max_total_time=300

Seed inputs live in `fuzz/seeds/`; copy them into `fuzz/corpus/<target>/` to start.

## Installing the tools

    cargo install cargo-deny cargo-audit cargo-fuzz --locked
    rustup toolchain install nightly --profile minimal
    rustup target add x86_64-unknown-linux-musl
    sudo apt install musl-tools
    export CC_x86_64_unknown_linux_musl=musl-gcc    # SQLite is compiled from C

The musl build is for static release binaries; glibc stays the default for development.

## Reporting a vulnerability

Please do not open a public issue for security problems.
Security contact: TODO(maintainer): set contact.
Include the affected commit, a reproducer (for parser bugs, the input bytes), and the
impact you expect. Fixes land with a regression test.
