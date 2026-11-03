# Security
Rusty is a security-first Rust rewrite of the Zimbra mail stack. This file says what the
code defends against today, what it does not, and how to check it.

## Threat model (current state)

Today Rusty is a set of libraries (core, store, directory, MIME); no network services yet.
An attacker who controls any of the following can:

- **Mail content (raw messages):** feed arbitrary bytes to the envelope parser. They can
  make it return empty fields. Property and fuzz tests found no panic, and parsed
  addresses are lowercase with no `<` or `>`.
- **Addresses / local parts:** submit arbitrary Unicode. Local parts use a strict allow
  list after NFC, lowercase, NFC: ASCII a-z, 0-9, RFC 5322 atext specials and interior
  dots, plus non-ASCII lowercase letters (category Ll) in the Latin blocks U+00DF..U+00FF
  (minus U+00F7), U+0100..U+024F and U+1E00..U+1EFF. Everything else is rejected:
  combining marks, other scripts, non-ASCII digits, symbols, emoji, spaces, controls,
  format characters, private use, unassigned, and compatibility letters (anything that
  changes under NFKC, e.g. ſ, ĳ, ǆ). Max 64 octets and
  64 characters. Checked for accounts, aliases, lists and external list members.
- **Blob names and hashes:** hashes must be 64 lowercase hex characters; the blob
  store and GC have tests against hostile names (see rusty-store tests).
- **Startup config file:** 64 KiB cap, errors never echo contents, a FIFO cannot hang startup.
- **Message size:** appends over the per-mailbox limit (default 50 MiB) get a quota error.

The project's own crates contain no `unsafe`: `#![forbid(unsafe_code)]` is checked in
every `crates/*/src/lib.rs`, and a scan rejects any `unsafe` token in `crates/*/src`.
The scan does not cover `tests/`, `benches/`, `build.rs` or `fuzz/`. Dependencies do
use `unsafe` (bundled SQLite C code, rustix, tokio, sqlx).

Local-part tradeoff: only Latin is accepted. Names in other scripts (Cyrillic, Greek,
Arabic, CJK, Indic, Hangul...) and emoji cannot be used yet, and ZWJ/ZWNJ are always
rejected. Other scripts will be opt-in per tenant later, under a single-script rule.
`İ` (U+0130) lowercases to `i` + a combining dot and is rejected.

## Invariants enforced by tests

- `workspace_forbids_unsafe` (rusty-core): the forbid attribute in real code (not a
  comment) in every `lib.rs`, and no `unsafe` token in `crates/*/src`.
- `rejects_zero_width_and_bidi_override_in_local_part` and the `local_part_allow_list_*`
  tests (rusty-directory): checked via `parse_address`, `create_account`, `create_list`,
  `add_alias` and external `add_list_member`; `josé.müller`, `straße`, `łukasz` and
  NFC/NFD `nguyễn.văn.an` pass, Cyrillic/Greek/fullwidth/combining/invisible are rejected.
- Mailbox size limit: `append_over_limit_is_quota`, `default_limit_is_50_mib`.
- Config: unknown keys, relative paths, out-of-range numbers and wrong-case log levels
  are rejected; oversize input is rejected both by the size check and by the bounded
  reader; FIFOs do not block.
- Address normalization is idempotent (fuzz target and proptest stand-in).

## Not covered yet

- No network services, so no TLS, authentication exposure or protocol parsing.
- No process sandbox (seccomp, namespaces, privilege drop).
- No encryption at rest. Argon2 hashing exists but no rate limiting or lockout yet.
- Homoglyph detection is partial. Cross-script mixing is impossible by construction, but
  intra-Latin look-alikes remain (dotless `ı` vs `i`, `ł`/`l`, `l` vs `1`). Follow-up:
  UTS #39 skeleton uniqueness per tenant.
- `parse_address` trims Unicode whitespace around the whole address and the domain and
  returns the canonical form. Protocol layers must reject padded input themselves.
- The Ll test follows the `unicode-general-category` crate's Unicode version.

## Dependency policy

- `deny.toml` (cargo-deny): crates.io only (no git or unknown registries), known
  vulnerabilities and yanked crates denied, unmaintained direct dependencies
  denied (transitive ones show up as `cargo audit` warnings), a license allowlist, wildcard version requirements denied, `openssl`/`openssl-sys` banned
  (TLS will use rustls).
- `cargo audit` runs in CI. `.cargo/audit.toml` ignores RUSTSEC-2023-0071 (`rsa`) with
  a written reason: it is only in `Cargo.lock` through the optional sqlx-mysql driver, never built.
  Re-check on every sqlx upgrade.

## Running the checks

    bash scripts/ci.sh

Stages: clippy (`-D warnings`), tests, cargo-deny, cargo-audit, tests and release build
for `x86_64-unknown-linux-musl`, a static-linking check of release binaries, and 60 s
of fuzzing per target (`FUZZ_SECS` overrides). The script stops at the first failure.
Without cargo-fuzz/nightly it prints a WARNING and runs only proptest stand-ins (weaker).
To fuzz one target by hand:

    cd fuzz && cargo +nightly fuzz run parse_address -- -max_total_time=300

Seed inputs live in `fuzz/seeds/<target>/`; ci.sh passes them to the fuzzer.

## Installing the tools

    cargo install cargo-deny cargo-audit cargo-fuzz --locked
    rustup toolchain install nightly --profile minimal
    rustup target add x86_64-unknown-linux-musl
    sudo apt install musl-tools
    export CC_x86_64_unknown_linux_musl=musl-gcc    # SQLite is compiled from C
The musl build is for static release binaries; glibc stays the default.

## Reporting a vulnerability

Do not open a public issue for security problems. Security contact: TODO(maintainer): set contact.
Include the affected commit, a reproducer (for parser bugs, the input bytes), and the
impact you expect. Fixes land with a regression test.
