# Rusty - one binary and one image per service (design, for the owner's approval)

Status: **proposal, no code changed.** Written 2026-11-14. Decisions already taken by the owner are in section 1;
section 10 lists what still needs an answer. The owner writes Vietnamese; answer in Vietnamese.

## 1. Decisions in force (not up for debate here)

- One separate binary and one image per service; no shared multi-call binary in production (a shared binary is one
  exploitation point). The all-in-one (`rusty d`, `--dev`) is dropped; development runs through compose with many containers.
- Fewer than 10 services in total, counting the database and the web parts.
- Postgres is the only database. Blobs will be S3 only; S3 credentials come from environment variables
  (plus a `*_FILE` form), per service, never from the database. Which S3 server is the owner's choice (open).
- The admin web is one service. The webmail is its **own** service (not the admin image). Webmail reads blobs through its
  backend (no presigned URLs, no client-side MIME parsing).
- The installer is a plain bash script. The admin web and the webmail each get their own design document.

## 2. The services

| # | Image (one binary each) | Ports | Role | Notes |
|---|---|---|---|---|
| 1 | `rusty-smtp-in` | 25 | unauthenticated inbound mail | the most exposed process; no AUTH, no DKIM key, no relay credentials |
| 2 | `rusty-submission` | 587, 465 | authenticated submission | password verification; enqueues outbound jobs; no DKIM key |
| 3 | `rusty-imap` | 143, 993 | IMAP | TLS key; IDLE fan-out |
| 4 | `rusty-outbound` | none (dials out) | delivery worker, DKIM signing, DSN | the only holder of DKIM private keys and relay credentials |
| 5 | `rusty-gc` | none | blob and row garbage collection, periodic jobs (`dns.check` later) | singleton by lease (section 7.3) |
| 6 | PostgreSQL | 5432 (internal) | database | external image, pinned digest |
| 7 | `rusty-admin` | 443 (via edge) | admin web (server-rendered), W2 | own design document |
| 8 | `rusty-webmail` | 443 (via edge) | webmail BE+FE | own design document, later |
| 9 | HAProxy | 25, 80, 110, 143, 443, 465, 587, 993, 995 | edge (TLS, PROXY v2, limits) | already built and tested; the owner's `prost` proxy is a candidate for later |

Count: 9 containers. S3 is external and not counted. In the development compose the edge is left out (ports published
directly), so a dev stack with an S3 server is also 9.

`rustyctl` stays its own binary and image (`rusty-ctl`): the one-shot `db migrate` job and operator commands. It is the only
place that holds the migrator role's credentials. It is not a long-running service and is not in the count.

## 3. Crate layout

Today `crates/rusty` is both the library and the multi-call binary (`daemon.rs` composes everything; `cli.rs` parses
`rusty d|ctl|healthcheck`). The protocol crates (`rusty-smtp`, `rusty-imap`, `rusty-svc`, `rusty-store`, `rusty-directory`)
already contain all service logic behind traits and facades, so the split is another composition root, as the crate
comment says.

```
crates/rusty-runtime/     NEW library, extracted from crates/rusty: config loading, store open (roles), TLS provider,
                          logging, signals, health probe, Context building. No service logic, no binary.
crates/rusty/             REMOVED at the end (its tests move, see section 8).
bin/rusty-smtp-in/        thin main: Role::SmtpIn  -> listener 25 + InboundHandler
bin/rusty-submission/     thin main: Role::Submission -> 587/465 + InboundHandler::submission + JobOutbound
bin/rusty-imap/           thin main: Role::Imap -> 143/993 + ImapHandler + change listener
bin/rusty-outbound/       thin main: Role::Outbound -> OutboundWorker + DKIM keyring + relay secrets
bin/rusty-gc/             thin main: Role::Gc -> GcService + periodic jobs
bin/rusty-ctl/            existing rusty-ctl crate as its own bin
```

Each `bin/*` depends only on the crates its service needs. Examples: `rusty-imap` must not depend on the DKIM signer or on
`mail-auth`; `rusty-smtp-in` must not depend on `rusty-imap`; only `rusty-outbound` links the DKIM keyring. A CI gate checks
this from `cargo tree` (allow-list per binary), in the spirit of the existing `scripts/check-service-crates.sh`.

Honest limit: `rusty-smtp-in` and `rusty-submission` share `InboundHandler` in `rusty-smtp`, so the AUTH code is compiled into
the port-25 binary even though no listener uses it. Splitting that code behind two cargo features
(`inbound` / `submission`) is a follow-up inside `rusty-smtp`; until then the port-25 binary refuses an authenticated
listener by configuration (`check_startup` already does) and a gate asserts it.

## 4. Configuration

One file per service (`/etc/rusty/<service>.toml`), plus environment for secrets. `StartupConfig::for_role(Role)` accepts only
the sections that belong to the role and **rejects the others** (an `imap.toml` with `[outbound]` is an error), so a service
cannot be handed a secret it does not use. Shared keys (`hostname`, `[store]`, `[tls]` where used, `[limits]`, logging) are
allowed everywhere. Secrets (database password, S3 keys, DKIM key paths, relay credentials) are only read by the role that
needs them; the Debug and log redaction rules that exist stay.

## 5. Database roles (least privilege per service)

Today there are three Postgres roles: `rusty_migrator` (DDL), `rusty_app` (all runtime DML) and `rusty_ro`. Per-service roles
split `rusty_app` into `rusty_smtp_in`, `rusty_submission`, `rusty_imap`, `rusty_outbound`, `rusty_gc` (`rusty_admin` later).
`deploy/db/roles.sql` and `crates/rusty-store/grants_pg.sql` get one grants block per role.

I did **not** derive the privilege matrix cell by cell yet; guessing it would be wrong in places (for example the delivery
path and the definer triggers of migration 0016). The way to derive it, and to keep it true:

1. List the statements each service issues (the repositories' SQL is in constants; the tests already count statements).
2. Generate the grants from that list, then run each service's conformance and process tests **connected as its own role**;
   a missing privilege fails a test, an extra one is found by the same generator.
3. Keep `rusty_app` until all five are green, then drop it.

## 6. S3 credentials per service (when the S3 blob store lands)

| Service | S3 rights |
|---|---|
| smtp-in, submission | put (new objects only; `If-None-Match: *` on put) |
| imap, webmail, outbound | get, ranged get |
| gc | list and delete |

This only works with an S3 server that can give a credential Put without Delete. The local benchmark (2026-11-14) showed
RustFS can (IAM policy on `s3:PutObject` without `s3:DeleteObject`) and SeaweedFS with a static `s3.json` cannot (its "Write"
includes delete). This is why the choice of the S3 server is a design input and stays the owner's decision.

## 7. What breaks when the all-in-one goes away, and the fix

### 7.1 IMAP IDLE (the delivery must wake the IMAP process)

`MailboxHub` is a per-process `watch` channel map; `ChangeNotifier::changed(mailbox, seq)` is called after a commit in the same
process. With separate processes, smtp-in delivers and imap never hears about it.

Fix, in order of preference:
- A trigger on `change_log` insert calls `pg_notify('rusty_mailbox', mailbox_id || ':' || seq)`. `NOTIFY` is delivered **at commit**,
  so it cannot announce a rolled-back change. The IMAP service keeps **one** `LISTEN` connection (sqlx `PgListener`) and feeds
  `MailboxHub::notify`. New migration **0017**.
- NOTIFY is not durable and a listener can reconnect, so IDLE must never depend on it for correctness: on (re)connect the
  listener re-notifies every subscribed mailbox, and IDLE sessions also re-check the change log on a slow timer (for example 30 s).
- `LISTEN` needs a session connection: with PgBouncer in transaction mode this one connection bypasses the pooler.

### 7.2 Outbound wake-up

`Wake` is a per-process `Notify`; the worker already polls every 2 s (`WorkerOptions.poll`, line 89 of `worker.rs`), so
correctness holds without it. A `jobs` insert trigger with `pg_notify('rusty_jobs')` and a listener in `rusty-outbound`
removes the poll delay. Optional, same mechanism as 7.1, can come after.

### 7.3 GC is a singleton

Today the singleton is a file lock on the blob volume (`.gc.lock`), which means nothing on S3 and nothing across hosts. Replace
by a lease row in Postgres with an epoch (the same fencing idea the job queue uses): the holder renews, a stale holder's
writes are rejected. New migration **0018**. The GC itself becomes database-driven (`blobs.refcount`, `zero_since`) with S3
`list`/`delete`; that part belongs to the S3 design.

### 7.4 Per-process limits multiply with replicas

`AuthThrottle`, `SubmissionLimiter`, `RateLimiter` and `MemoryBudget` are in memory per process (seams exist). N replicas mean
N times the limit. Acceptable for the first release; the edge's per-IP limits stay global; a Postgres-backed limiter is a
later item. To be written in the operator documentation.

## 8. Images, build, CI

- One `Dockerfile` with one build stage (zig cross-compile to musl, as today) and one final stage per service: distroless
  "static", numeric non-root user, the **single** binary of that service and nothing else. `docker build --target rusty-imap`.
- One version tag for all images, built from one commit; each image signed (cosign keyless) with an SBOM.
- `HEALTHCHECK` through the binary itself (`rusty-imap healthcheck`): distroless has no shell. Probe logic comes from
  `crates/rusty/src/health.rs`.
- New or changed gates: each image contains exactly one executable; per-binary dependency allow-list from `cargo tree`;
  per-binary symbol gate (no `rusty_imap` in smtp-in, no DKIM signing code outside outbound); `check-image.sh` runs per image;
  the Docker pin and release workflows build the matrix.
- Process-level tests of `crates/rusty` (`daemon.rs`, `cli.rs`, `ctl.rs`) become tests of `rusty-runtime`, started in-process
  per role against the test Postgres, plus one smoke test per image in CI. `local-run.sh`, `quickstart.sh` and
  `deploy/dev/rusty.compose.yaml` become a compose file with Postgres, an S3 server, and the five mail services.

## 9. Migration steps (each is a PR with tests first and a green CI; nothing ships half-way)

1. **Cross-process IDLE** (migration 0017, listener, resync): extend `smtp_wakes_idle` so smtp and imap are separate
   `Context`s over one database. This lands **first**: without it a split deployment silently loses IDLE.
2. **`rusty-runtime` extraction**: behaviour-neutral refactor; the all-in-one still works; all tests green.
3. **`Role` and per-role configuration** (`for_role`, rejection of foreign sections) with tests.
4. **Five thin binaries + multi-target Dockerfile + the new gates**; at the end `rusty d` and `crates/rusty` are removed
   and the dev compose replaces the dev profile.
5. **GC lease** (migration 0018).
6. **Per-service database roles** with each service's tests running as its own role.
7. **S3 blob store** (own design with the benchmark and the owner's S3 choice): trait change, DB-driven GC, ranged reads.
8. **Compose for development, installer script**, then the admin web and the webmail services.

Steps 1 to 3 are refactors under the existing tests; step 4 is the visible change.

## 10. Questions for the owner

1. Is it acceptable that smtp-in and submission share the `rusty-smtp` code (separate binaries and configs, AUTH code
   compiled into both until the feature split)?
2. Per-service Postgres roles in step 6 (recommended) or keep a single `rusty_app` for the first release?
3. The S3 server (decides whether the Put/Delete split in section 6 is enforced by S3 or only by network and code).
4. Is the `rusty-ctl` image (migrate job, operator commands) the right shape, with the migrator credentials only there?
5. Is a 2 s outbound poll acceptable for the first release (step 7.2 optional)?

## 11. What I verified and what I did not

Read and checked: `crates/rusty/src/daemon.rs` and `cli.rs`, `rusty-svc` `hub.rs`, `gc.rs`, `context.rs`, `plan.rs`,
`outbound/worker.rs` (the 2 s poll), `deploy/db/roles.sql`, `deploy/docker/Dockerfile`, `scripts/check-image.sh`.
Not verified: the exact per-service SQL privileges, whether a `change_log` trigger fits the existing write path and the
definer triggers without a measurable cost, PgBouncer behaviour with `LISTEN`, and the size of each final image.
