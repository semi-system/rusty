# Rusty - Usable all-in-one mail server (U1-U9)

Date: 2026-11-08. Status: draft for user approval. This is milestone **M1** of `2026-11-03-roadmap-zimbra-parity.md` (Giai doan 1, criteria under the table), re-cut for the new priority.

**Priority change (user, 2026-11-08):** "make it USABLE first; performance and security hardening later." Goal: a real person can read and send mail with Thunderbird (and a phone IMAP client) against a real domain, on one box, with one binary.

Language of this plan is English (the product UI is English-only, roadmap 0c). Sizes are in **agent-sessions** (one implementer session including its own fix loop; reviewer sessions are counted separately). They are estimates, not measurements: the project has no productivity data for protocol work yet.

## 0. What changes versus the roadmap

| Roadmap said | This plan |
|---|---|
| Decision 6: "Postgres backend first; M1 runs on Postgres" | **Superseded for M1.** M1 runs on SQLite behind the existing facades. Postgres is a backlog item, not a prerequisite. |
| P-02 separate `store` service + `RemoteMailbox` | **Not now.** In-process store. The seam is the `Mailbox`/`Directory`/`JobQueue` facades (section 2); `RemoteMailbox` stays the future swap point. |
| P-03 edge (HAProxy, TLS termination) | **Not now.** Each service terminates TLS itself with rustls. |
| P-04/P-05/P-06 IMAP, inbound, submission | U6/U7, U3, U4. |
| P-10/P-11 (MTA, DKIM) | Only the usable subset: U5 (queue, MX, retry, DSN, DKIM sign). SPF/DKIM verify inbound is log-only. |
| Process model spec: `rustyd` supervisor + child daemons | **One process.** `rustyd` runs all services as tasks. Multi-call binary `rusty` (`rusty d`, `rusty ctl`, or argv[0] `rustyd`/`rustyctl`). No supervisor logic, no child processes. |

### Postgres: paused, explicitly (deferred backlog)
The following stay in `2026-11-03-postgres-backend.md` as **deferred backlog, not cancelled, not started**: **PG6 review fixes**, **PG7**, **PG8**, **PG8a** (PgBouncer layer). Nothing in U1-U9 may depend on them. The only obligation this plan puts on Postgres is: do not break what already compiles/passes (the `postgres` feature and the shared parity tests keep building); new trait methods added here are implemented on BOTH backends with conformance tests (section 1b rule 1, section 2.3), which is the only Postgres work this plan includes and which needs the existing local Postgres test setup (`scripts/pg-up.sh`). The uncommitted edits another agent has in `2026-11-03-postgres-backend.md` are not touched by this plan.

## 1. Decisions recorded (made by the user, 2026-11-08)

1. Single all-in-one process `rustyd`; multi-call binary `rusty`.
2. In-process store: no separate store service yet. No edge/HAProxy yet. TLS is terminated by the services with **rustls** using a configured cert/key, or an auto-generated **self-signed dev cert** (explicit opt-in).
3. **SQLite** backend behind the existing `Mailbox`/`Directory` facades.
4. Admin via `rustyctl` CLI (create domain/account/alias, set password, list). No admin API/console yet.
5. IMAP: `imap-codec` (pin 1.0.0 unless evaluation says otherwise) + our own server logic.
6. SMTP: `smtp-proto` + our own server.
7. Outbound: simple persistent queue on the existing job queue + `hickory-resolver` MX lookup + `mail-send` or own client, STARTTLS.
8. DKIM signing with `mail-auth` (needed for deliverability to big providers); SPF/DKIM verification inbound, **log-only** at first.
9. Webmail UI (Svelte 5) is a LATER milestone, after Thunderbird works.
10. English-only UI. Domain = tenant isolation must stay intact; the existing cross-mailbox/tenant isolation tests must not regress.
11. Fuzz every parser we own (cargo-fuzz). `cargo-deny` licence check before adding any crate. No AGPL (Sieve `sieve-rs` and Stalwart server crates stay excluded).

## 1b. Architecture invariants (do not trade for speed)

Binding rule from the user (2026-11-08): keep the existing model so later features stay easy to add; do NOT bend the architecture to save time. Every task's acceptance criteria include "section 1b holds"; a reviewer rejects a task that breaks any of these even if every functional test is green.

1. **Protocol servers talk only to the facades/repository traits** (`Mailbox`, `Directory`, `JobQueue`, `Gc`, `BlobStore`, `Authenticator`) - never `SqliteStore`, a pool, or raw SQL. If a protocol needs an operation the traits lack, it is added to the trait for **both backends** with conformance tests in the shared parametrised suite (section 2.3), not worked around. A later `store` service (`RemoteMailbox`) or Postgres swap must need zero protocol-code change.
2. **One crate per protocol** (`rusty-smtp`, `rusty-imap`, `rusty-ctl`; `rusty`/`rustyd` is only a thin composition root). Inside each protocol crate three layers stay separate: a **codec/parser** layer (bytes <-> typed commands/responses, no I/O, no state), a **session/state** layer (sans-I/O state machine: input events in, output actions out, no sockets, no clock reads except through an injected clock), and a thin **I/O driver** (tokio streams, TLS, timeouts). This is what makes them fuzzable and movable into separate daemons (process-model spec: multi-call `rusty <service>`, one process per service later) without rewrites.
3. **Constructor injection, no globals.** A service is built from `(typed config section, trait objects)`; no `static`/`lazy` singletons, no global config or registries. All-in-one `rustyd` and a future multi-daemon deployment are just different composition roots.
4. **Existing semantic rules stay as designed:** capability/role model; domain = tenant isolation; DB-clock, idempotency (`DeliveryId`) and lock-ordering rules; SQLSTATE-style typed errors mapped at the protocol edge (`Error::Busy` -> SMTP `451 4.3.0` / IMAP `NO [UNAVAILABLE]`, `Quota` -> `552`/`NO [OVERQUOTA]`/`[LIMIT]`, `Invalid` -> `5xx`/`BAD|NO`, `NotFound` -> `550 5.1.1`/`NO [NONEXISTENT]`, storage -> `451`/`NO` + never leak internals). One shared mapping table per protocol, tested.
5. **Existing CI gates apply to the new crates with no exemptions:** raw-db gate, parametrised both-backend tests, test-knobs / test-fixtures / mutation-hook gates, `cargo-deny`, `cargo-audit`, clippy `-D warnings`, fuzz smoke. If a gate does not fit a new crate, the gate is extended, not skipped.
6. **Shortcuts are allowed only as DEFERRED FEATURES**, documented in section 4's deferred list and with a seam that exists in code now (trait, hook or config key) and is listed in the table below. A structural shortcut (reaching around a trait, merging layers, a global, an SQLite-only code path in a service) is not allowed.

### Seams: where each deferred feature plugs in
| Deferred feature | Seam that exists in the new code (built in this plan) |
|---|---|
| Edge / HAProxy (TLS termination, PROXY protocol) | `rusty-svc` listener takes an `Accept` layer (`PeerInfo{addr, tls}`): default `DirectTls`; a `ProxyProtocol` accepter fills the same `PeerInfo`. TLS is an I/O-driver concern, session layers only see `PeerInfo.tls`. Config key `[smtp|imap] proxy_protocol = false` is parsed and rejected-if-true now |
| `store` service / `RemoteMailbox` | Services hold `Arc<dyn MailboxRepo/DirectoryRepo/JobRepo>` (via `Mailbox::open_with`, `Directory::with_repo`, `JobQueue::with_repo`); the composition root is the only place that builds them from `Store::open` |
| Postgres backend | `StoreConfig` variant already exists; services never name a backend; new ops land on both backends (2.3) |
| KMS / OpenBao | `SecretSource` trait in `rusty-svc` (`file` implementation now) used for TLS key, DKIM keys and relay password; KMS is another implementation |
| ACME / `certd`, per-domain certs | `TlsProvider` resolves certs through a `CertResolver` trait keyed by SNI (single-cert implementation now, SIGHUP reload); `certd` supplies another resolver |
| rspamd / ClamAV | `InboundFilter` pipeline trait in `rusty-smtp` called after DATA and before delivery, returning `Accept | Reject | Quarantine(folder) | Tag(headers)`; now a no-op list holding only the log-only SPF/DKIM checker |
| Webmail / JMAP | A future `rusty-jmap` crate is just another service over `Context`; `MailboxHub` change notifications and `changes_since` are the push/sync primitives; no webmail assumptions in IMAP/SMTP crates |
| ACL / shared folders | IMAP session resolves folders through a `FolderResolver` trait (personal namespace only now), so `NAMESPACE`/ACL later swaps the resolver and the store grants; the `Mailbox` facade stays per-mailbox |
| Quota enforcement | `QuotaPolicy` trait consulted by delivery/append in the service layer plus the existing per-mailbox/limits knobs in `MailboxLimits`; no-op policy now (size cap only) |
| Rate limiting (per tenant/account/IP) | `RateLimiter` trait in `rusty-svc` with in-memory implementation (auth throttle, submission limits); later a store/Valkey-backed implementation and per-tenant keys |
| Outbound relay per tenant / source IP selection | `OutboundRoute` trait (`route(sender_domain, rcpt_domain) -> Direct{bind} | Relay{...}`); global config implementation now |
| Metrics / audit | `ServiceEvents` trait (connection, auth result, delivery) with a `tracing` implementation now |

## 2. Where we start from (verified by reading the code, 2026-11-08)

### 2.1 What exists
Crates: `rusty-core` (ids, strict TOML config, errors), `rusty-store` (SQLite + feature-gated Postgres, `FsBlobStore`, mailbox facade, job queue, GC), `rusty-mime` (envelope parse, header limits), `rusty-directory` (domains, accounts, aliases, lists, Argon2 `LocalAuthenticator`, capabilities, config layers). `tools/` holds edge test tooling; `fuzz/` has two targets (`parse_address`, `parse_envelope`). `rustls`/`tokio-rustls` are already workspace dependencies. `Mailbox` already has what IMAP needs at the storage level: per-folder gapless UIDs, `deliver` with `DeliveryId` idempotency (+ fenced variants for job leases), flags/keywords, move, expunge, batches, `changes_since` change log, paged `list_folder`, `get_raw_ref`.

### 2.2 The "repository traits make a Postgres swap cheap" claim: verified, with exceptions
`crates/rusty-store/src/repo/mod.rs` defines operation-granularity traits `DirectoryRepo`, `MailboxRepo`, `JobRepo`, `GcRepo`; `Store::open(StoreConfig)` returns them as `Arc<dyn ...>` for both SQLite and Postgres, and `Directory::with_repo`, `Mailbox::open_with`, `JobQueue::with_repo` take them. `BlobStore` is a trait. So the claim **holds for the data path**: services written against the facades never see the backend, and a Postgres swap is a config change plus the PG backlog.

**NOT yet behind traits (found by reading):**
- Convenience constructors that take the concrete `SqliteStore`: `Directory::new`, `Mailbox::open`, `JobQueue::new`. Services must use the `*_with_repo`/`open_with` forms (a clippy/grep check in U1 enforces it).
- Free functions in `gc.rs` (`purge_deleted`, `purge_deliveries`, `purge_capabilities`, ... ) take `&SqliteStore`; `Store::sqlite()` exists as an escape hatch "for facades not converted yet". GC scheduling in `rustyd` must go through `Gc` built from `Repos.gc`, not those functions.
- Missing operations the services need, which do not exist on any backend yet: enumerate domains and accounts (for `rustyctl list`), per-account folder subscriptions (IMAP SUBSCRIBE/LSUB), `copy_message` (IMAP COPY: a real trait op on both backends, sharing the blob by hash), body/header search (IMAP SEARCH: header and flag criteria from summaries; body criteria scan blobs inside the facade with a hard cap, failing closed), exposing UIDVALIDITY on `FolderInfo` (the column exists on `folders`; to confirm it is surfaced). There is also no `BODYSTRUCTURE`/section extractor anywhere (built in `rusty-imap`, section U6).
- Directory and mailbox data share one SQLite file today; the system-DB/user-DB split in the Postgres spec is not implemented. Fine for M1 (backup = one DB file + the blob dir).

### 2.3 Rule for new store methods
(Revised after the user's binding rule, section 1b.) A missing operation is added to the existing trait (`DirectoryRepo`/`MailboxRepo`/`JobRepo`) and implemented on **both** SQLite and Postgres, with a test in the shared parametrised suite that runs on both backends (the existing `check-parametrised` gate applies), the raw-db and mutation-hook gates applying to the new SQL, and a `migrations_pg` twin for every SQLite migration. No default "unsupported" bodies, no SQLite-only shortcuts. Migration numbers are reserved by task to avoid clashes between parallel worktrees: `0010` U2 (if needed), `0011` U6 (subscriptions), `0012` U5 (if the queue needs columns), each with the same number in `migrations_pg` (the PG numbering is currently at 0001, so the PG twin of the first new migration is written as the next PG number; the reservation table maps SQLite number to PG number in the PR).

## 3. Architecture of the usable server

```
                    rustyd (one process, tokio)
  :25  smtp-in  --\
  :587 submission --+--> rusty-svc::Context { Directory, Authenticator, MailboxHub,
  :465 submission(TLS)|                        JobQueue, BlobStore, Config, TlsAcceptor, Limits }
  :143 imap     --/                                   |
  :993 imap(TLS)                                      v
  outbound worker  --claims--> JobQueue ----> Mailbox/Directory facades ----> SQLite + FsBlobStore
  gc / purge ticker
```

New crates (separate so lanes can be built in parallel worktrees):
- `rusty-svc` - shared service plumbing: typed config sections, TLS acceptor (cert/key reload, self-signed dev), listener loop with connection limits, `CancellationToken` shutdown, auth-failure throttle, `MailboxHub` (open a `Mailbox` for an account and broadcast "mailbox changed" for IDLE), `Context`. Landed by U1; every other crate depends on it and on nothing else new.
- `rusty-smtp` - SMTP server logic for both inbound (25) and submission (587/465), plus the outbound client/delivery worker (U5).
- `rusty-imap` - IMAP server logic over `imap-codec`.
- `rusty-ctl` - `rustyctl` commands as a library (so the multi-call binary and tests share them).
- `rusty` (bin) - multi-call entry: `rusty d` / `rustyd`, `rusty ctl` / `rustyctl`.

TLS: one cert/key pair (SAN covers the mail hostname) loaded from PEM files in the config, reloaded on SIGHUP. Per-domain certificates are deferred (tenant-tls spec). `dev_self_signed = true` generates an in-memory/dev-dir cert with a loud log warning and is refused unless explicitly set. No cert and no dev flag: listeners that accept authentication do not start (fail closed).

Outbound path for local recipients: delivery goes straight to `Mailbox` (no loopback SMTP). The server does **not** write a Sent copy; Thunderbird APPENDs it itself.

## 4. Minimum security floor (stays in the usable-first phase)

These are not "hardening"; they are what makes it safe to put a port on the internet. Every task's acceptance criteria include the ones that apply.

1. **TLS-only authentication.** No AUTH/LOGIN accepted on a plaintext connection; STARTTLS capability advertised first (`LOGINDISABLED` on IMAP, no `AUTH` in EHLO before TLS). Port 25 never offers AUTH. Implicit-TLS ports 465/993.
2. **No plaintext passwords on the wire**, and none in logs, errors, panics or metrics. AUTH payloads and message bodies are never logged (test greps captured logs).
3. **Argon2 hashes** (exist). Constant-ish timing on unknown users (exists in `LocalAuthenticator`). Minimal in-memory auth-failure throttle per (peer IP, identity), with a global cap on concurrent auth attempts.
4. **Parser limits everywhere:** command line length, literal size, header size (exists in rusty-mime), recipients per message, commands per session, session idle timeout, connections total and per IP, message size (config, default 50 MiB, mapped to `552` / IMAP `NO [LIMIT]`).
5. **No panics on input:** `#![forbid(unsafe_code)]`, no `unwrap`/index on peer data in service crates (clippy `unwrap_used`, `indexing_slicing` denied there), every parser has a fuzz target, and a panic in a session task is caught, logged, and drops only that session.
6. **Fail closed:** any internal error on the auth or delivery path yields a temporary failure (`451`/`NO`/`BYE`), never an accept without a commit. Port 25 is **not an open relay**: unauthenticated `RCPT` to a non-local domain is `554 5.7.1`.
7. **Domain = tenant isolation** preserved: account resolution only through `Directory`; every IMAP object id from the wire is resolved through the session's own `Mailbox` (which already scopes by mailbox id); submission sender must belong to the authenticated account. Cross-tenant matrix test in U8 at protocol level.
8. **No backscatter:** inbound rejects at `RCPT`/end-of-DATA, never accepts-then-bounces. DSNs are only generated for mail that an authenticated local user submitted.
9. **Inbound `Authentication-Results`/forged headers:** strip any inbound `Authentication-Results`, `Received-SPF`, `X-Rusty-*` claiming our hostname before storing.
10. Secrets on disk (TLS key, DKIM keys, relay password file) are files with mode `0600` owned by the service user; startup refuses group/world-readable key files (same posture as the existing `blob_dir` checks). Relay passwords are read from a file, never inline in the config.
11. `cargo-deny` + `cargo-audit` stay green in CI; licence evaluation precedes every new crate.

### Explicitly deferred (not in this plan)
HAProxy edge and PROXY protocol; KMS/OpenBao (keys live in 0600 files for now); ACME/`certd` and per-domain certificates (cert is configured by hand or Let's Encrypt via an external tool, documented in U9); rspamd/ClamAV; PgBouncer and all Postgres work (PG6 review fixes, PG7, PG8, PG8a); sharding; Valkey; admin API/console; webmail; JMAP; POP3; Sieve/vacation; DMARC enforcement and ARC; MTA-STS/DANE; per-tenant rate limits/quotas beyond a size cap; search index; ACL/shared folders; IMAP QRESYNC.

## 5. Task list

Legend: **Review** = S (strong independent reviewer: parser/auth/concurrency) or L (light: one pass, no separate reviewer session unless CI is red). **Size** in agent-sessions, implementer only.

| Task | Title | Crate(s) | Size | Review | Depends on |
|---|---|---|---|---|---|
| U1 | Skeleton: config, `rusty-svc`, TLS, logging, shutdown, dev quickstart | rusty-core, rusty-svc, rusty (bin) | 3 | L (TLS/loader part S, 0.5) | - |
| U2 | `rustyctl` admin + bootstrap | rusty-ctl, rusty-directory, rusty-store | 3 | L | U1 |
| U3 | SMTP inbound (port 25) | rusty-smtp | 4 | S | U1 |
| U4 | SMTP submission (587/465) | rusty-smtp | 3 | S | U3 skeleton |
| U5 | Outbound queue, MX delivery, DSN, DKIM sign | rusty-smtp, rusty-ctl | 5 | S | U1; U4 for end-to-end |
| U6 | IMAP core | rusty-imap, rusty-store (gaps) | 11 | S (two passes) | U1 |
| U7 | IMAP extras for Thunderbird/phones | rusty-imap | 4 | S | U6 |
| U8 | End-to-end acceptance and isolation matrix | tools/acceptance (new), fuzz | 4 | L (matrix S, 0.5) | U3-U7 |
| U9 | Packaging, docs "first mail in 15 minutes", backup/restore | deploy, docs, rusty-ctl | 3 | L | U8 |

**Sum: 40 implementer sessions, about 6-7 strong-review sessions: 46-47 total. Plan for 37-58.** The IMAP pair (U6+U7 = 15) has the widest error bar (+/-50%): real-client quirks only show up in U8/manual Thunderbird.
**Critical path with three parallel lanes:** U1 (3) -> U6 (11) -> U7 (4) -> U8 (4) -> U9 (3) = about 25 sessions of wall-clock in the best case; realistically 28-32 with review/fix loops and merge friction. Lane A (U3 -> U4 -> U5 = 12) and lane C (U2 = 2, U8 harness early = 2) fit inside the IMAP lane.

### Dependency graph and parallelism
```
U1 --+--> U2 ------------------------------+
     +--> U3 --> U4 --> U5 (+DKIM) --------+--> U8 --> U9
     +--> U6 --> U7 ----------------------+
```
- After U1 merges, **three worktrees run in parallel**: `rusty-smtp` (U3->U4->U5), `rusty-imap` (U6->U7), `rusty-ctl` (U2, then the U8 harness skeleton and U9 documentation drafts).
- Merge-conflict rules: (a) U1 pins all workspace dependency versions that later tasks will need in `[workspace.dependencies]` (empty placeholders are not allowed; add the real crate once its evaluation passes: see each task's evaluation step, which may add a dependency line, so these are single-line additive edits, rebase-friendly); (b) the store/directory trait additions are made only in U2 (`list_domains`, `list_accounts`) and U6.0 (subscriptions, copy, search helpers, uidvalidity), each as the **first commit of its branch, merged to main before the rest of the branch** so lanes never edit `repo/*.rs` concurrently; (c) migration numbers reserved in 2.3; (d) `Cargo.lock` conflicts are resolved by regeneration, never by hand-merge.
- U4 needs only the U3 session skeleton (codec loop, limits, TLS upgrade, state machine), which U3 lands as its first commit.
- U5's worker can be developed and tested against a fake DNS resolver and a fake remote MTA before U4 exists; only the final "submit -> queue" test needs U4.

### Common rules for every task
- TDD: write the tests listed as "must fail first", watch them fail for the stated reason, then implement. Tests written afterwards do not count.
- Library step first: before adding a crate, a short evaluation note in the task's PR description (licence via `cargo deny check licenses`, maintainer/last release, transitive dependency count, `cargo audit`, our fuzz run on its entry point, replacement boundary). No evaluation, no dependency. Do not commit an evaluation file unless the answer is "no".
- Existing CI (`scripts/ci.sh`) stays green, including the isolation tests of `rusty-store`/`rusty-directory`. New crates join CI (`fmt`, `clippy -D warnings`, tests, deny, audit).
- Services use only the facades (no `SqliteStore` in service crates); section 1b (invariants and seams) is a review checklist item on every task, and the seam traits it lists are created by the task that owns the corresponding code (U1: `Accept`, `SecretSource`, `CertResolver`, `RateLimiter`, `QuotaPolicy`, `ServiceEvents`; U3: `InboundFilter`; U5: `OutboundRoute`; U6: `FolderResolver`) with a trivial implementation and a test that the default is wired through the trait.

---

### U1 - Skeleton: config, `rusty-svc`, TLS, logging, shutdown, quickstart (3)
**Files/crates:** extend `rusty-core::config` (keep strict, unknown keys rejected); new `crates/rusty-svc`, `crates/rusty` (bin, multi-call); `rusty.example.toml`; `deploy/dev/` quickstart.

**Config additions (all optional with safe defaults):** `hostname` (required for non-dev), `[tls] cert_file, key_file, dev_self_signed`, `[smtp] listen_25, listen_587, listen_465, max_message_bytes, max_recipients, max_connections, ...`, `[imap] listen_143, listen_993, ...`, `[outbound] ...` (filled by U5), `[limits]`. Defaults: standard ports; a `--dev` profile uses high ports (2525/5587/4465/1143/9993) on 127.0.0.1.

**Interfaces:**
```rust
pub struct Context { directory: Directory, auth: Arc<dyn Authenticator>, hub: MailboxHub,
                     jobs: JobQueue, blobs: Arc<dyn BlobStore>, cfg: Arc<Config>, tls: TlsProvider }
pub trait Service { async fn run(self, ctx: Context, shutdown: CancellationToken) -> Result<()>; }
pub struct MailboxHub;   // open_for(account) -> Mailbox ; subscribe(mailbox_id) -> watch::Receiver<u64>; notify(mailbox_id, seq)
pub struct TlsProvider;  // acceptor() -> TlsAcceptor ; reload() ; from_config(&TlsConfig)
pub struct AuthThrottle; // check(ip, identity) / record_failure / record_success
pub trait OutboundEnqueue { async fn enqueue(&self, msg: OutboundMessage) -> Result<()>; } // defined here, implemented in U5
```
**Libraries:** `tracing-subscriber` (MIT), `tokio-util` for `CancellationToken` (MIT), `rcgen` for the dev cert (MIT OR Apache-2.0; evaluate, dev-profile only, behind a cargo feature so a production build can omit it), PEM parsing via `rustls-pki-types` (already in tree). `arc-swap` only if an `RwLock<Arc<..>>` proves insufficient.

**TDD - must fail first:**
1. config: unknown key rejected; production profile without `hostname`/cert and without `dev_self_signed` fails to start authenticated listeners (test asserts exit/err, not a plaintext fallback).
2. TLS: a rustls client handshakes against `TlsProvider` with a fixture cert; after `reload()` with a new cert, new connections see it, old ones continue; a key file with mode 0640 is refused.
3. shutdown: with an open idle test connection, cancelling the token closes the listener, drains the session within the deadline, and `Store` closes cleanly (SQLite WAL checkpointed).
4. `MailboxHub`: `notify` wakes subscribers; slow subscribers cannot block notifiers.
5. log hygiene test: a fake session logs a line containing a password/AUTH blob through the redaction helper and the captured output does not contain it.
6. clippy/grep check: no `SqliteStore` in service crates.
**Acceptance (section 1b invariants hold, plus):** `rusty d --dev` starts, creates the SQLite file and blob dir if missing (with the existing marker/permission rules), listens on the dev ports, logs JSON or text (config), exits 0 on SIGTERM; a `deploy/dev/` quickstart (plain binary script, plus an optional compose file) works from a clean checkout.
**Fuzz:** config parser (`fuzz_config`), PEM loader entry.
**Review:** light overall; the cert/key loader and permission checks get a strong-reviewer pass (about half a session).

### U2 - `rustyctl` admin and bootstrap (3)
**Files:** `crates/rusty-ctl`; store/directory additions `list_domains`, `list_accounts(domain)`, `list_aliases(domain)` on `DirectoryRepo` (both backends + shared parametrised tests, per 2.3); migration `0010` only if something other than SELECTs is needed.
**Commands:** `rustyctl domain add|list|disable`, `account add <addr> [--password-stdin|--prompt]`, `account list [domain]`, `account set-password`, `account disable|enable`, `alias add|list`, `dkim generate <domain> [--selector s]` (key generation lives in U5; the command is stubbed here and wired in U5), `check` (config + DNS hints), later `backup`/`restore` (U9). Passwords come from stdin/TTY, never argv. Output is plain text and `--json`.
**Bootstrap:** first run on an empty database with `rusty d --bootstrap-admin` is NOT needed; instead `rustyctl domain add example.com` + `account add postmaster@example.com` is the whole bootstrap, and the `postmaster@` and `abuse@` aliases are created automatically per new domain (RFC 5321 requires postmaster).
**Concurrency:** `rustyctl` opens the same SQLite file while `rustyd` runs (WAL + busy timeout already exist); test that both can write.
**TDD - must fail first:** create domain+account then `Authenticator::verify` succeeds and a wrong password fails; password never appears in argv-based help/errors; IDN domain is normalised via the existing `normalize_address`; duplicate domain/account is a clean error; `account add` into a nonexistent domain fails; running ctl against a DB while a writer holds the lock waits (busy) rather than corrupting.
**Acceptance (section 1b invariants hold, plus):** after U1+U2 a human can create a domain and two accounts from the shell. **Fuzz:** none (no parser beyond clap; address parsing already fuzzed). **Review:** light.

### U3 - SMTP inbound, port 25 (4)
**Files:** `crates/rusty-smtp` (`session`, `inbound`, `data`, `limits`, `authres`).
**Library evaluation (first step):** `smtp-proto` 0.2.5: licence (Apache-2.0 OR MIT per roadmap table, re-check with cargo-deny), request parser coverage for EHLO/HELO/MAIL (SIZE, BODY, SMTPUTF8)/RCPT/DATA/BDAT/RSET/NOOP/QUIT/VRFY/STARTTLS/AUTH, behaviour on oversized/garbage lines (must not allocate unboundedly: our line reader enforces the 512/1000-byte-style cap before the library sees input), and whether it parses server replies (needed for the client in U5). Fallback if rejected: own parser for the ~12 verbs (small; fuzzed anyway).
**Behaviour:**
- Greeting `220 <hostname> ESMTP`; EHLO capabilities `PIPELINING 8BITMIME SMTPUTF8 SIZE <max> ENHANCEDSTATUSCODES CHUNKING(only if BDAT implemented; otherwise omit) STARTTLS`. No AUTH on port 25.
- `MAIL FROM`: accepts `<>`; validates syntax; enforces `SIZE` param against max (`552 5.3.4`).
- `RCPT TO`: resolve through `Directory::resolve` (account, alias, list expansion with the existing member cap); non-local domain `554 5.7.1 relay denied`; unknown local user `550 5.1.1`; suspended account `550 5.2.1`/`452`; cap recipients (default 100) with `452 4.5.3`.
- `DATA`/`BDAT`: dot-unstuffing reader with hard size cap (`552 5.3.4` and drain/close), reject bare-LF-only abuse consistently (accept bare LF by normalising to CRLF; refuse NUL bytes), prepend `Received:` (no recipient address in it for multi-recipient mail) and `Authentication-Results:` (after SPF/DKIM log-only checks in U5's `mail-auth` once available; U3 ships the header slot with `none`).
- Delivery: one `Mailbox::deliver` per local recipient (list expansion deduped by account), `DeliveryId` = hash(envelope-sender, recipient-account, **original DATA bytes before we prepend headers**), so a remote MTA retrying after a crash between commit and `250` lands on the existing message instead of duplicating (this is what makes the U8 crash test pass). Documented consequence: two byte-identical messages to the same recipient within `DELIVERY_RETENTION_SECS` collapse into one; acceptable.
- Reply `250` only **after** all deliveries commit; if any recipient fails temporarily after others succeeded, accept per-recipient semantics through `LMTP`-style handling is not available in SMTP, so the rule is: all-or-`451` for the transaction, relying on idempotent `DeliveryId` for the retry. Postmaster: `postmaster@<local domain>` always resolves (alias created by U2) even for domains with no other accounts.
- Inbound SPF/DKIM verification: log-only at first (`tracing` event with result, no rejection). Mail from our own domain arriving unauthenticated on 25 is accepted and logged (reject policy is a deferred hardening item).
- Limits: command line 1000 bytes, session idle 5 min, `DATA` idle 10 min, 100 commands without a transaction, 5 transactions/connection default, per-IP concurrent connections, global connection cap.
**TDD - must fail first** (using a real TCP loopback test client): relay to a foreign domain refused before DATA; unknown user 550; oversize message 552 and nothing stored; message with a lone `.` line / dot-stuffed lines stored byte-exact; two recipients on one account stored once; list expansion respects the cap; kill/replay: delivering the same `DeliveryId` twice stores once; `Received` header added and a forged `Authentication-Results` stripped; STARTTLS upgrade resets state per RFC 3207; commands after `QUIT`; 8-bit/SMTPUTF8 round trip; slow-loris client is cut at the idle limit.
**Fuzz targets:** `fuzz_smtp_command` (line reader + command parse), `fuzz_smtp_data` (dot-unstuffing and size cap state machine), `fuzz_smtp_path` (MAIL/RCPT path and params).
**Acceptance (section 1b invariants hold, plus):** `swaks` from another host delivers into a local mailbox; no open relay (swaks to foreign domain without auth refused). **Review:** strong, after the session skeleton and again at U3 end.

### U4 - SMTP submission, 587 STARTTLS + 465 implicit (3)
**Files:** `rusty-smtp::submission`, reusing U3's session skeleton.
**Behaviour:** `AUTH PLAIN` and `AUTH LOGIN` only after TLS (advertise only then); `Authenticator::verify`; throttle through `AuthThrottle`; after auth: **sender rule** - `MAIL FROM` must be the authenticated account's primary address or one of its aliases (resolved through `Directory`), else `553 5.7.1`; header `From:` is not rewritten but checked: if the header From address is not an identity of the account, reject (`550 5.7.1`) - Thunderbird sends consistent identities so this is safe and blocks spoofing within the domain. Recipients: local via `Directory::resolve`, remote accepted and queued. Message gets `Message-ID` and `Date` if missing, a `Received` header without the client's IP exposed beyond what is conventional. Local recipients: `Mailbox::deliver` directly with a job-independent `DeliveryId`; remote recipients: `OutboundEnqueue::enqueue` (U5) **before** replying `250`; for mixed recipients, both must be durable before `250`.
**Rate limits (minimal):** per-account max messages/hour and max recipients/message (config defaults 200/h, 100), per-connection transaction cap; exceeding gives `452 4.5.3`/`451 4.7.1`. Auth-failure throttle from U1.
**Library:** none new.
**TDD - must fail first:** AUTH before STARTTLS is refused and not advertised; AUTH PLAIN with wrong password fails and counts toward throttle; authenticated user cannot send as another account or another tenant's address (`553`), nor via header From mismatch; mail to a local recipient arrives in their Inbox; mail to a remote recipient creates exactly one durable job before `250` (kill the process right after `250` and the job is still there); 465 implicit TLS works and 465 plaintext is dropped; rate limit hit; AUTH LOGIN base64 edge cases (empty, oversized, invalid base64 -> `501`, `*` cancel).
**Fuzz targets:** `fuzz_smtp_auth` (SASL PLAIN/LOGIN decoders). **Acceptance (section 1b invariants hold, plus):** Thunderbird/`swaks --tls --auth` sends mail to a local account. **Review:** strong (auth).

### U5 - Outbound queue, delivery, DSN, DKIM signing (5)
**Files:** `rusty-smtp::outbound` (`queue`, `resolve`, `client`, `dsn`, `dkim`), `rusty-ctl dkim generate`.
**Spikes first (0.5 session, results decide design):**
1. **Blob GC vs queued blobs.** The queued message body is stored in the `BlobStore` and the job payload carries its hash (job payload size is capped by `MAX_JOB_PAYLOAD_BYTES`). Determine from `gc.rs`/migration `0002_blob_zero_since` whether an unreferenced-but-recent blob can be collected while a job is retrying for days. Default decision if so: config validation requires `gc_grace_seconds >= outbound.max_age + 1 day`. Alternative (if grace alone is unsafe): store the queued message as a hidden "Outbox" system folder message of the sender (referenced, so GC-safe, filtered from IMAP LIST). Pick by test, record the result in the PR.
2. **`mail-send` vs own client.** Evaluate `mail-send` 0.7.0: it must (a) connect to a chosen IP/port while validating the certificate against the MX hostname, (b) support opportunistic STARTTLS with our rustls (`ring`, no `openssl`; deny.toml bans openssl), (c) expose per-recipient result codes (needed for DSN), (d) not pull a heavy or duplicate TLS stack. If any fails, write the client ourselves on `smtp-proto` response parsing (about 300 lines; fuzz the response parser).
3. **`mail-auth` 0.15.1:** crypto backend features compatible with our `ring` stack, DKIM sign (RSA-2048 and ed25519) and SPF/DKIM verify APIs, resolver injection for tests. `hickory-resolver` is its resolver dependency, so MX lookup reuses it (one DNS stack).
**Behaviour:**
- Job kind `outbound.send`, one job per (message, destination domain) so retries and fan-out are independent; payload: `{blob, envelope_from, rcpts[], first_queued, domain_tenant}`; the job `domain` field is the **sender's tenant** so the existing per-domain pending cap applies.
- Worker: claim with lease and heartbeat (existing `JobQueue`), `fence_check` before any irreversible step, resolve MX (fallback to A/AAAA per RFC 5321 implicit MX; null-MX `.` means permanent failure), sort by preference, try each with STARTTLS opportunistic (no downgrade attack protection yet: deferred, MTA-STS/DANE), per-destination connection concurrency cap, per-recipient result handling (partial success: only failed recipients stay in the job).
- Retry: backoff 5m, 15m, 30m, 1h, 2h, 4h, then every 4h up to 5 days (config); `4xx`/network errors retry, `5xx` permanent. On permanent failure or expiry: generate a **DSN** (RFC 3464 multipart/report, plain text part plus message/delivery-status plus original headers only) and deliver it to the sender's local Inbox via `Mailbox::deliver` with a deterministic `DeliveryId` (job id) so a crash cannot double-bounce. Never DSN for a null-sender message.
- DKIM: per-domain key in `<data>/dkim/<domain>.<selector>.pem` (0600), selector and enabled flag in domain config via `set_domain_config`; sign `From`, `To`, `Subject`, `Date`, `Message-ID`, `MIME-Version`, `Content-Type`, relaxed/relaxed; default RSA-2048 for the widest receiver compatibility, ed25519 optional second signature (open question 3). `rustyctl dkim generate` prints the DNS TXT record to publish.
- Relay placeholder: `[outbound.relay] host, port, tls = starttls|implicit|none, username, password_file` (global only now). When set, all remote mail goes there instead of direct MX delivery; connection test on `rustyctl check`. Per-tenant/per-domain relays and failover are deferred (roadmap P-06/P-10 note). Dedicated outbound source IP: `[outbound] bind_address` placeholder, applied to the client socket.
- Inbound verification (promised in U3): wire `mail-auth` SPF/DKIM verify on port 25 and into `Authentication-Results`, still log-only.
**TDD - must fail first** (fake resolver, fake remote MTA on loopback with a scripted state machine): MX order and implicit-MX fallback; null MX is permanent; `4xx` then `250` leaves exactly one delivery; partial recipient failure; `5xx` creates one DSN in the sender's Inbox and exactly one even if the worker is killed and re-run; crash after remote `250` but before `complete` does not lose the job (documented duplicate-risk: at-least-once, remote may get it twice) - the test asserts no loss and DSN-not-sent; lease expiry lets another worker take the job and the old worker's `fence_check` fails; DKIM signature verifies with `mail-auth` against a test DNS record and fails after body tampering; TLS to a remote with an invalid certificate still delivers (opportunistic) but is logged; backoff schedule table; queue cap per tenant enforced; GC spike result enforced by config-validation test.
**Fuzz targets:** SMTP response parser (if own client), DSN input header copier, queue payload decoder. **Acceptance (section 1b invariants hold, plus):** from a real domain with DNS set up, a message to a Gmail/Outlook test address is delivered and shows `dkim=pass` (manual check in U8/U9). **Review:** strong (concurrency, crash safety).

### U6 - IMAP core (11)
**Files:** `crates/rusty-imap` (`codec`, `session`, `state`, `auth`, `select`, `fetch`, `structure`, `search`, `store_cmds`, `append`, `list`); store gap work **U6.0** as the first commit.
**U6.0 store gaps (2 sessions inside the 11, both backends):** `subscribe/unsubscribe/list_subscriptions` (migration `0011`), `copy_message(s)` op, surface `uid_validity` and `uid_next` and a per-folder message count/unseen count in `FolderInfo`/a `folder_status` op if the existing ones are not enough (check first; do not add if present), search helper ops limited to what summaries cannot answer (address/header fields are available from blobs: scan with cap). All additions follow 2.3.
**Library evaluation (first, half session):** `imap-codec` 1.0.0 (MIT OR Apache-2.0): spike with a table of the real command lines Thunderbird, K-9 and Apple Mail send (capture from a stock Thunderbird against a throwaway server, or take from imaptest) - `CAPABILITY`, `ID`, `ENABLE`, `STARTTLS`, `LOGIN`, `AUTHENTICATE PLAIN` with and without SASL-IR, `LIST` with `RETURN (SPECIAL-USE)` and `LIST-EXTENDED`, `STATUS`, `FETCH` with `BODY.PEEK[HEADER.FIELDS (...)]<partial>`, `UID FETCH`, `STORE`, `APPEND` with literals and LITERAL+ (`{n+}`), `UID MOVE`, `UID EXPUNGE`, `IDLE`/`DONE`, `SEARCH` with charset, `NAMESPACE`. Verify which cargo features cover MOVE, UIDPLUS, ENABLE, LITERAL+/-, IDLE, SPECIAL-USE and whether literal handling (continuation request before reading data, size cap before allocation) is under our control. **Decision rule:** if 1.0.0 handles the table with feature flags, pin `=1.0.0`; if a needed extension exists only in 2.x alpha, pin an exact 2.0.0-alpha only if its alpha status is accepted in the PR (open question 1), else write the missing command parsing ourselves on top of 1.0.0. Library wraps behind our own `Command`/`Response` types? No: use its types directly inside `rusty-imap` only; the public surface of the crate is `serve(Context, stream)`, so a replacement stays local.
**Behaviour:** greeting with capabilities; `CAPABILITY` (`IMAP4rev1`, `STARTTLS`, `LOGINDISABLED` before TLS, `AUTH=PLAIN` after); implicit TLS on 993; `LOGIN`, `AUTHENTICATE PLAIN` through `Authenticator` + `AuthThrottle`; `LIST`/`LSUB`/`SUBSCRIBE`/`UNSUBSCRIBE`; `CREATE`/`DELETE`/`RENAME` mapped to folder create/delete/move (hierarchy delimiter `/`; system folders 1-5 cannot be deleted/renamed; names validated by `validate_folder_name`); `SELECT`/`EXAMINE` returning `FLAGS`, `EXISTS`, `RECENT`, `UIDVALIDITY`, `UIDNEXT`, `UNSEEN`; `STATUS`; `FETCH` of `FLAGS UID INTERNALDATE RFC822.SIZE ENVELOPE BODYSTRUCTURE BODY[...]` incl. `HEADER`, `HEADER.FIELDS(.NOT)`, `TEXT`, `MIME`, numbered parts, `<partial>`, `.PEEK`, `RFC822*`; `STORE` (+/-FLAGS, `.SILENT`) mapping system flags to `Flags` and other flags to keywords; `EXPUNGE`, `CLOSE`, `UNSELECT`; `SEARCH` basic (flags, `UID`, sequence sets, `SINCE/BEFORE/ON`, `LARGER/SMALLER`, `FROM/TO/CC/SUBJECT/HEADER`, `TEXT/BODY` with a scan cap that fails closed with `NO [LIMIT]`, `NOT/OR`); `UID` variants of `FETCH/STORE/SEARCH/COPY`; `APPEND` (flags, internal date, size cap, literal handling, `Mailbox::append`); `COPY`/`MOVE` (Thunderbird uses `UID MOVE` when advertised); `NOOP`/`LOGOUT`; unsolicited `EXISTS`/`EXPUNGE`/`FETCH FLAGS` via `changes_since` between commands.
**Session state:** per-session UID list snapshot for the selected folder (sequence-number mapping), updated from `changes_since`; paging through `list_folder` with `MAX_PAGE`; no unbounded memory for a 100k-message folder (summaries only, bodies streamed from `BlobStore`).
**Own code, own fuzz:** `structure` (ENVELOPE, BODYSTRUCTURE, section extraction from `mail-parser` offsets, with depth/part-count/size limits and a "treat as single text/plain part on parse failure" fallback so a hostile MIME never panics or hangs), `search` (key evaluator), sequence-set expansion with a cap, literal/continuation state machine, FETCH response writer (output validated by decoding it with imap-codec in a property test).
**TDD - must fail first:** per command a golden transcript test against a loopback TCP server with a real client library (Python `imaplib` via the U8 harness is for U8; here use `async-imap` as a dev-dependency only if its licence passes, otherwise raw transcripts); plus: `LOGINDISABLED` blocks LOGIN before TLS; wrong password then throttled; `SELECT` another account's folder id is impossible (names only, resolved in own mailbox); `UID FETCH 999999` returns no data without error; APPEND over limit returns `NO [LIMIT]` and drains/closes the literal safely; sequence number to UID mapping stays consistent after a concurrent delivery and a concurrent `EXPUNGE` from another session; `STORE` to an expunged message is reported correctly; `FETCH BODY[]<0.100>` partial boundaries; nested multipart and broken MIME fixtures; 2 GB literal announcement rejected before allocating; command pipelining; an oversized command line closes the session.
**Fuzz targets:** `fuzz_imap_command` (our framing + literals + imap-codec), `fuzz_imap_structure`, `fuzz_imap_search_eval`, `fuzz_imap_seqset`.
**Acceptance (section 1b invariants hold, plus):** a stock Thunderbird can add the account over IMAPS (self-signed accepted manually), shows folders and messages, reads, flags, deletes, moves, drafts. **Review:** strong, two passes (after FETCH/structure; at end).

### U7 - IMAP extras Thunderbird and phones need (4)
**`IDLE`:** wait on `MailboxHub` `watch` for the selected mailbox plus a 30-second `changes_since` poll as backstop (covers writes from `rustyctl` or other processes); send `* n EXISTS`/`EXPUNGE`/`FETCH`; terminate on `DONE`; enforce server-side idle timeout (30 min) and per-user concurrent session cap (default 20) since phones hold IDLE connections. `MailboxHub::notify` is called by the SMTP delivery path and by IMAP mutations (wrap at one place in `rusty-svc`, not per call site).
**`UIDPLUS`** (`APPENDUID`, `COPYUID`, `UID EXPUNGE`), **`MOVE`** (RFC 6851, atomic via `Mailbox::move_many`, `COPYUID` + `EXPUNGE`), **`NAMESPACE`** (single personal namespace `""` with `/`; no shared/other-users, consistent with "no cross-tenant"), **`LITERAL+`** (non-synchronising literals with the same size caps), **`ID`** (server name only; do not disclose version; log client ID), **`ENABLE`** (accept `UTF8=ACCEPT`/`CONDSTORE` gracefully, enable nothing unsupported), **`SPECIAL-USE`** (`\Sent \Drafts \Trash \Junk` from folder roles, `\Archive` omitted until it exists), `UNSELECT`, `STATUS=SIZE` skipped, `CHILDREN`, `LIST-EXTENDED` basic, `SASL-IR`.
**CONDSTORE: skip in M1, with justification:** Thunderbird and K-9 work without it (they fall back to flag polling), it adds `MODSEQ` to every message and to STORE/FETCH semantics, and the change log supports adding it later (`changes_since` seq is a natural modseq). Gate: advertise it only if the U8 Thunderbird checklist shows a real client needs it (open question 4). `QRESYNC` stays deferred.
**TDD - must fail first:** IDLE receives an `EXISTS` within 1 s of an SMTP delivery in-process and within 35 s of a write by another process; `DONE` ends IDLE and normal commands resume; cap exceeded; MOVE leaves no copy in the source and yields correct `COPYUID`; `APPENDUID` matches the real UID; non-sync literal over limit closes the connection; `ENABLE` of unknown extension is ignored not errored; LIST with SPECIAL-USE returns attributes for the five system folders; NAMESPACE response format golden.
**Fuzz:** extends `fuzz_imap_command` corpus with these commands; new `fuzz_imap_idle_state` for the IDLE/DONE state machine. **Review:** strong (concurrency in IDLE).

### U8 - End-to-end acceptance (4)
**Files:** new `tools/acceptance` (Rust test runner that starts `rusty d --dev` in a temp dir) plus scripts under `tools/acceptance/scenarios/` (Python 3 stdlib `imaplib`/`smtplib`/`ssl`, `swaks`) - CI-friendly, no GUI. Does not edit `scripts/` or `crates/`; it is invoked from CI by a one-line call added in a later change when the user allows touching `scripts/ci.sh` (list as a follow-up, not part of this plan's commit).
**Scenarios (all must pass in CI on Linux, targeting the musl build too):**
1. Happy path: `rustyctl` creates domain+2 accounts; `swaks` delivers to account A on port 25; imaplib lists INBOX, fetches headers/body, sets `\Seen`, copies, moves, deletes + expunges, creates a folder, appends a draft; account A submits to account B over 587 STARTTLS with AUTH and B receives it; account A submits to an external address captured by a fake MX (resolver override) with a valid DKIM signature.
2. Protocol conformance basics: capability/greeting transcripts; `imaptest`-style subset if the tool installs reliably in the CI image (otherwise documented as not used - roadmap section 4.3 asks for this note).
3. **Crash-kill mid-delivery:** loop N times, `kill -9 rustyd` at randomised points while swaks delivers K messages (client retries on `451`/connection drop as a real MTA would); after restart assert: every message that got `250` is present exactly once (compare counts and message hashes), no message present twice, no partially written message, SQLite integrity check and blob consistency (`FsBlobStore` heal report) clean. Same kill test for a queued outbound message: after restart it is still delivered exactly once to the fake MX or exactly-at-least-once with no loss (documented) and one DSN at most.
4. **Cross-tenant isolation matrix at protocol level:** accounts A1 and A2 in `a.test`, B1 in `b.test`. Over IMAP: A1 cannot SELECT/LIST/STATUS/FETCH/COPY/APPEND-to/search anything of A2 or B1, including by guessed folder ids/UIDs/names/namespace tricks and `UID` ranges; over SMTP: A1 cannot send as B1 or A2 (envelope or header From); unauthenticated 25 to non-local rejected; submission to a suspended account's address is refused; list expansion cannot cross domain; queue jobs and DSNs of tenant A never appear for B. Also run the existing store/directory isolation tests unchanged.
5. Abuse: oversized lines/literals/messages, slow-loris, 1000 connections, AUTH brute force hits throttle, malformed MIME bomb (deep nesting) does not hang IMAP FETCH BODYSTRUCTURE.
6. Shutdown: SIGTERM during active sessions drains within the deadline and leaves no `.lock`/WAL issues.
**Manual Thunderbird checklist** (`tools/acceptance/THUNDERBIRD.md` or in the U9 doc): add account (autoconfig is manual: server names, SSL/TLS 993, STARTTLS 587), accept self-signed cert, folder subscription and special folders mapping (Sent/Drafts/Trash/Junk), receive while IDLE, send with attachment (>5 MB), reply/forward, flag, move, delete to Trash, empty Trash, rename folder, search in body, offline sync after reconnect, K-9/phone repeat of the core steps, a Gmail round trip checking `dkim=pass`/`spf=pass`. Failures become issues against U6/U7.
**Fuzz in CI:** each target runs a short smoke (60 s) in CI; long runs are scheduled manually.
**Review:** light overall; the isolation matrix gets a strong review (about half a session).

### U9 - Packaging, docs, backup/restore (3)
- **musl static binary** (`x86_64-unknown-linux-musl`, already in `deny.toml` targets): `rusty` multi-call; verify TLS (ring) works, no `openssl`; size noted.
- **Minimal Dockerfile and compose** under `deploy/` (single container, non-root user, volumes `/var/lib/rusty` for DB+blobs+dkim, `/etc/rusty` for config and certs, ports 25/465/587/143/993, `cap_add: NET_BIND_SERVICE` or high-port mapping); sample `rusty.example.toml` extended; the compose file does not include Postgres or HAProxy.
- **Docs "first mail in 15 minutes"** (`docs/first-mail.md`): install, create domain+account, certificate (own cert files, or Let's Encrypt via an external tool plus SIGHUP reload, or dev self-signed), **DNS records for a real domain:** `A` for `mail.<domain>`, `MX`, `SPF` TXT (`v=spf1 mx -all`, plus the dedicated IP), `DKIM` TXT (printed by `rustyctl dkim generate`), `DMARC` TXT (`p=none` to start, `rua` optional), **PTR/rDNS** for the sending IP matching the hostname, and the firewall/ISP notes (many hosts block outbound 25: relay setting as the workaround); Thunderbird setup steps; troubleshooting (`rustyctl check`, logs).
- **Backup/restore** of SQLite + blobs: `rustyctl backup <dir>` uses `VACUUM INTO` (consistent snapshot while running) **first**, then copies the blob directory (blobs are immutable and content-addressed; copying after the DB snapshot guarantees every blob referenced by the snapshot exists, extra newer blobs are harmless; GC grace protects blobs from being deleted during the copy), plus `dkim/` and config (not the TLS key unless `--include-secrets`); `rustyctl restore <dir>` into an empty data dir with the same marker/permission checks, then runs a blob-reference verification. Test: backup under load, restore into a fresh dir, run U8 happy-path read checks against the restored data, and a deliberate missing-blob case fails loudly.
**TDD - must fail first:** restore refuses a non-empty target; backup of a DB mid-write passes `PRAGMA integrity_check`; restored `Mailbox` returns identical message hashes; docs lint: every command in the doc is executed by a test script against `--dev` (so the doc cannot rot).
**Review:** light.

---

## 6. Fuzz target inventory (cargo-fuzz, in the existing `fuzz/` crate)
`fuzz_config`, `fuzz_smtp_command`, `fuzz_smtp_data`, `fuzz_smtp_path`, `fuzz_smtp_auth`, `fuzz_smtp_response` (own client only), `fuzz_dsn_headers`, `fuzz_queue_payload`, `fuzz_imap_command`, `fuzz_imap_structure`, `fuzz_imap_search_eval`, `fuzz_imap_seqset`, `fuzz_imap_idle_state`, plus the two existing targets. Each has a seed corpus of real transcripts (captured Thunderbird/swaks sessions) and runs a short smoke in CI.

## 7. Risks
| Risk | Likelihood / impact | Mitigation |
|---|---|---|
| IMAP real-client quirks (Thunderbird, K-9, Apple Mail) dwarf the estimate | High / schedule | Capture real client transcripts in U6's library spike; U8 manual checklist early (after U6 lands) rather than at the end; keep U7 list flexible |
| `imap-codec` 1.0.0 lacks an extension we need; 2.x is alpha | Medium / U6 | Evaluation gate with a decision rule; own parser for the missing verb; boundary kept inside `rusty-imap` |
| Deliverability: new IP reputation, no PTR, ISP blocks 25, Gmail spam-foldering despite DKIM/SPF | High / "usable" | U9 DNS docs, relay setting, DKIM RSA-2048; manual Gmail/Outlook test; honest note that reputation is not a code problem |
| Queue blob GC race (queued body collected while retrying) | Medium / data loss | U5 spike 1 with a failing test first |
| Crash/duplicate semantics (SMTP is at-least-once) | Medium / correctness | `DeliveryId` over original DATA; documented duplicate window; U8 kill test |
| `mail-send`/`mail-auth` pull a second TLS or crypto stack, or openssl | Medium / supply chain | Evaluation step; own client fallback; `deny.toml` bans openssl |
| Open relay or spoofing bug in a hurry-driven build | Low / severe | Security floor items 6-7, strong review of U3/U4, U8 matrix |
| SQLite single-writer contention (IMAP flag storms + deliveries + queue) at small scale | Low / latency | WAL + busy timeout exist; batch ops used; benchmark deferred by decision |
| Parallel worktrees collide on `Cargo.lock`, migrations, trait files | Medium / merge pain | Rules in section 5 (reserved migrations, trait additions first and alone) |
| Both-backend rule slows U2/U6.0 and needs a working local Postgres | Medium / schedule | Sized in (U2 3, U6 11); only the few new operations are affected; PG6-PG8a hardening stays deferred |
| Single self-signed/hand-made cert is awkward for real users | Medium / usability | U9 documents Let's Encrypt via external tool + SIGHUP reload; ACME in `rustyd` is a deferred item to promote if users hit this |
| Scope creep from "usable" (webmail, search speed, spam filtering) | High | The deferred list in section 4 is the contract; additions need user approval |
| TLS client-cert verification for outbound is opportunistic (downgrade possible) | Accepted | Deferred (MTA-STS/DANE); logged |

## 8. Exit criteria for the plan (M1-usable)
1. Fresh machine: musl binary + config + DNS, `rustyctl domain add`, `account add`, and in 15 minutes an external sender (Gmail) reaches the mailbox, and the user replies and Gmail shows `dkim=pass`.
2. Thunderbird and one phone client read, move, delete, flag, search and send over IMAPS/submission (U8 checklist signed).
3. U8 scenarios green in CI, including crash-kill and the isolation matrix; the pre-existing isolation tests unchanged and green.
4. No item of the security floor missing; every owned parser has a fuzz target with a seed corpus.
5. Backup and restore demonstrated.

## 9. Backlog parked by this plan
Postgres: PG6 review fixes, PG7, PG8, PG8a (hardening only; the new trait methods of U2/U6 are NOT parked, they ship on both backends). Edge/HAProxy plan `2026-11-03-edge-haproxy.md` stays ready but unscheduled. After M1: ACME/`certd`, per-domain certs, rspamd/ClamAV, DMARC enforcement, per-tenant limits, admin API and console, then the Svelte 5 webmail (needs JMAP or an internal API; decide then), POP3, Sieve (own interpreter, AGPL crate excluded).
