# Security (Rusty Mail Server)
Rusty Mail Server is a security-first Rust rewrite of the Zimbra mail stack. This file says what the
code defends against today, what it does not, and how to check it.

## Threat model (current state)
Rusty is early-stage software, not production ready. It now runs network services (SMTP inbound
and submission, IMAP) in a single process, but without a process sandbox and without the planned
edge layer in front. An attacker who controls any of the following can:

- **Network input to SMTP/IMAP:** the protocol parsers are ours (a library codec confined to an
  adapter module), bounded (line, literal, command, message, recipient and time limits), fuzzed
  and fail closed; AUTH is accepted only over TLS; failed logins are throttled per resolved
  account and per source (in memory, per process).
- **Mail content (raw messages):** feed arbitrary bytes to the envelope parser. Only the
  header section (up to the first empty line, at most 256 KiB, else `Invalid`) is parsed,
  so cost is independent of body size, MIME part count and nesting. Output fields carry no
  C0/C1 controls and are capped: subject 2048 bytes, from/to 4096 bytes and 100 whole
  addresses, message ids 998 bytes (else dropped), last 32 references.
- **Addresses / local parts:** submit arbitrary Unicode. After NFC, lowercase, NFC only ASCII
  a-z, 0-9, RFC 5322 atext specials, interior dots and lowercase Latin letters (Ll,
  U+00DF..U+024F, U+1E00..U+1EFF) pass; everything else is rejected. Max 64 octets and 64
  characters. Other scripts will be opt-in per tenant. Applies to accounts, aliases, lists.
- **Blob names and hashes:** hashes must be 64 lowercase hex characters; the blob store
  and GC have tests against hostile names, symlinks and FIFOs (rusty-store tests).
- **Startup config file:** 64 KiB cap, errors never echo contents, a FIFO cannot hang startup.
- **Sizes:** appends over the per-mailbox limit (default 50 MiB) get a quota error;
  `list_folder` and `changes_since` return at most 1000 entries per call.

Errors are fixed short reasons and never echo caller input (`error_leak` test). The
`source()` chain of a storage error may hold raw backend text and must not reach clients.

No `unsafe` in the project crates: `#![forbid(unsafe_code)]` in every `lib.rs`, and a scan
rejects `unsafe` in `crates/*/src`. Dependencies do use `unsafe` (sqlx, rustix, tokio).

## Files and permissions
- Run as its own account with **umask 077**; data directories are 0700. The database is PostgreSQL: the
  daemon connects as an unprivileged role (never a superuser, enforced at start-up), over verified TLS
  unless an explicit, logged override is set; the password comes from a 0600 file, never from the URL.
- The blob root is created only by `FsBlobStore::init` (0700, `.rusty-blobs` marker).
  `open` refuses a missing, unmarked, symlinked or group/world-writable root. Blob files
  are 0600, written via random `O_EXCL|O_NOFOLLOW` temp files, renamed, then fsynced.

## Invariants enforced by tests
- `workspace_forbids_unsafe` (rusty-core): forbid attribute in every `lib.rs`, no `unsafe`.
- `local_part_allow_list_*`, the exhaustive allow-list test; normalization is idempotent.
- Parser bounds and sanitization: `rusty-mime/tests/bounds.rs`, fuzz oracle and proptest.
- Blob store: modes, init/open, symlink/FIFO refusal, 2,000 put-vs-GC races. Schema:
  `schema_matches_snapshot` pins every table, index and trigger.
- Mailbox size limit (`append_over_limit_is_quota`); config rejects unknown keys, relative
  paths and bad numbers.
- Idempotent delivery (`rusty-store/tests/deliveries.rs`): a replayed delivery id returns
  the original and writes nothing; a reused id with different content is `Conflict`;
  `purge_deleted` keeps a delivered message until its delivery row has expired.
- Job queue (`rusty-store/tests/jobs.rs`): lease plus fencing epoch; a zombie worker's
  heartbeat, complete and fail are refused (`Conflict`); claims touch O(1) rows and settle
  at most 100 exhausted jobs; 10,000 pending jobs per domain; payload 64 KiB, nesting 64.
  The contract is at-least-once: every step, including DB effects, must be idempotent.
  `complete_in` commits the effect and the completion atomically (rolled back for a zombie).
- Capabilities (`rusty-directory/tests/capabilities.rs`): 256-bit random token; only
  `SHA-256(tag || token)` is stored (`only_hash_is_stored`); malformed, unknown, expired,
  revoked and wrong-role tokens all give the same `Ok(None)`; `set_password` and a
  non-active `set_status` revoke all of the account's tokens in the same transaction; at
  most 64 active per account (oldest revoked); TTL clamped to 24 h; the token is never in
  errors or `Debug`.
- Batch operations (`rusty-store/tests/batch.rs`): `move_many`, `expunge_many` and
  `deliver_many` run in one transaction, all-or-nothing, 1..=`MAX_BATCH` (1000) items;
  `deliver_many` is also capped at 64 MiB in total. A list expands to at most 10,000
  members, else `Quota`, never a truncated list (`rusty-directory/tests/directory.rs`).
- Protocol version negotiation picks the highest common version; non-overlapping or
  malformed ranges are `Invalid` with a fixed message (`rusty-core/tests/proto.rs`).
- Edge (`deploy/EDGE-DECISION.md`): only the edge publishes ports; all containers non-root,
  read-only, `cap_drop: ALL`, no runtime socket; backend links are verified mTLS plus PROXY
  v2; client PROXY headers cannot spoof the source IP; unknown SNI refused; per-IP caps hold.
Limits: bind-mounted test keys, 300 s idle on pass-through ports, CN fallback, no cap on  chunked bodies, plaintext at the edge on 465/993/995/443.

## Not covered yet
- Network services exist but are young: protocol surface reviews are ongoing, there is no
  external penetration test yet, and no process sandbox (seccomp/Landlock, privilege drop).
- Limiters (per-account submission, auth throttle, memory budget) are in memory and per
  process; the planned edge layer (HAProxy) and the separate store service are not in front yet.
- No encryption at rest (database and blob volume rely on host/disk protection).
- Capability tokens are wiped best effort (no `zeroize` crate); copies may remain in memory.
  Positive capability validations must never be cached once a cache exists (revocation).
- The `_at(.., now)` clock twins are `doc(hidden)` but still public; move them behind a
  test-clock feature before any network API exists.
- Tenant model, ACL sharing, KMS and the edge layer are specified in `docs/` but not built.
- Intra-Latin homoglyphs remain (`ı`/`i`, `ł`/`l`); UTS #39 skeletons are a follow-up.
- `parse_address` trims whitespace around the address; protocol layers must reject padding.

## Dependency policy
- `deny.toml` (cargo-deny): crates.io only, vulnerable and yanked crates denied, unmaintained
  direct dependencies denied, license allowlist, no wildcards, `openssl` banned (TLS will use
  rustls). The toolchain is pinned in `rust-toolchain.toml`.
- `cargo audit` runs in CI. `.cargo/audit.toml` ignores RUSTSEC-2023-0071 (`rsa`): only in
  `Cargo.lock` via the optional sqlx-mysql driver, never built. Re-check on sqlx upgrades.

## Running the checks
Run `bash scripts/ci.sh`. Stages: clippy, tests, cargo-deny, cargo-audit, musl tests/build,
static-link check, compose hardening check, fuzz lockfile check, 60 s fuzzing per target (`FUZZ_SECS`; proptest
stand-ins without cargo-fuzz/nightly). By hand: `cd fuzz && cargo +nightly fuzz run parse_address`.
Tools: `cargo install cargo-deny cargo-audit cargo-fuzz --locked`, nightly, `musl-tools`
(`CC_x86_64_unknown_linux_musl=musl-gcc`). Edge suite: `EDGE_SUITE=1 bash scripts/ci.sh`.

## Reporting a vulnerability
Do not open a public issue for security problems. Security contact: mornngstr1@gmail.com
(provided by the maintainer on 2026-11-03). Include the affected commit, a reproducer (for
parser bugs, the input bytes) and the expected impact. Fixes land with a regression test.
