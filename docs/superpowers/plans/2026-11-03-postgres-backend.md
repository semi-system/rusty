# Postgres Backend Implementation Plan (Part 1: infrastructure, schema, vertical-slice spike)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Rusty a Postgres 18 backend (user decision: Postgres is the production source of truth, SQLite stays for dev/test/small installs and the system DB) with provable behavioural parity to the existing SQLite backend, starting with test infrastructure, the Postgres schema and a vertical-slice spike that decides how to abstract the backends.

**Architecture:** Backend-neutral repository traits at operation granularity (spec Postgres-Valkey §3d: "ranh giới backend mức phương thức"); the facades (`Mailbox`, `Directory`, `JobQueue`, GC, capabilities) keep validation, limits, blob IO and MIME parsing, and call the repository inside a transaction; each backend implements the repository with its own SQL. Postgres runs at READ COMMITTED; every write transaction takes the mailbox row lock first. Part 2 (repository extraction for all modules, conformance suite, concurrency tests, benchmark) is written AFTER the spike report, because the spike decides the trait boundary.

**Tech Stack:** Rust, sqlx with the `postgres` feature (rustls), Postgres 18 in Docker (`postgres:18`, record the digest; the amd64 digest seen on 2026-11-03 was `sha256:f4b419e350fd98d4a8429603295208976dea4c8fb8326d818d346d3e29f1db6f` — re-resolve and verify at implementation time, do not trust this note), plpgsql triggers, proptest.

**Specs:** `docs/superpowers/specs/2026-11-03-rusty-postgres-valkey-design.md` (§3, §3c, §3d), `docs/superpowers/specs/2026-11-03-rusty-scale-targets-design.md` (§1b target 100,000 mailboxes / 1,000,000 mails per day; §1c asynchronous replication; §8 rules), `docs/superpowers/specs/2026-11-03-rusty-distributed-model-design.md` (§4 consistency).

## Global Constraints

- SQLite stays the default backend; ALL existing tests (376+) stay green after every task; refactors change no behaviour. Backends are cargo features: `sqlite` (default) and `postgres`.
- Workflow: edit on `<repo>`, build/test via `bash scripts/wsl-cargo.sh ...` (Docker and Postgres run in WSL). WSL `/tmp` is wiped on restart: logs go to `$HOME`.
- Postgres tests need a real server. `RUSTY_TEST_PG_URL` points at it (an admin role able to CREATE DATABASE); each test run creates an isolated database and drops it in a guard. If the variable is unset the Postgres tests print a loud SKIPPED notice; `scripts/ci.sh` stage `postgres` FAILS when Docker is available but the tests did not run (never a silent skip).
- Container for tests: `postgres:18` pinned by digest in `deploy/images.lock`, bound to 127.0.0.1 only, random password per run (never committed), data on tmpfs, resource limits, no published port beyond loopback.
- Parameterised queries only; no SQL built by concatenating caller data; errors are fixed strings (the existing `map_db_error` contract), mapped by SQLSTATE: custom codes `RS001` = Conflict and `RS002` = Invalid raised by our triggers, `23505`/`23503`/`23514`/`23502` as in the SQLite mapping, `40001`/`40P01` (serialization/deadlock) -> retried a bounded number of times then `Storage`.
- Time: unix seconds from the database clock (`extract(epoch from statement_timestamp())::bigint`) with the same `_at(now)` test twins; never the Rust clock for correctness decisions.
- Locking rule (document in the module docs): every write transaction starts with `SELECT ... FROM mailboxes WHERE id = $1 FOR UPDATE` (the Postgres equivalent of SQLite `BEGIN IMMEDIATE` + touching the mailbox row first), then folders, then messages; `blobs` rows are always touched in ascending hash order (see Review Focus).
- `#![forbid(unsafe_code)]`, no panics on hostile input, no raw SQL error text in `Error` messages, nothing secret in logs.
- No cross-mailbox constraint or join is added (scale-targets §3, §8 rule).

## Review Focus

- Counter gaplessness and monotonicity (`next_msg_id`, `next_change_seq`, `next_uid`, `next_folder_id`, `uid_validity`) under REAL concurrency on Postgres (separate connections, multi-thread runtime), including batches.
- Deadlocks: two concurrent transactions inserting messages that reference the same blobs in different orders update the same `blobs` rows (refcount triggers) and can deadlock on Postgres (impossible on SQLite's single writer). Required: ascending-hash ordering of blob upserts in every code path + bounded retry on `40P01`/`40001`; a test that forces the interleaving.
- Every SQLite CHECK, FK policy (`NO ACTION` end-of-statement vs `RESTRICT`), trigger and unique index has a Postgres equivalent AND a negative test that fails if the equivalent is removed (mutation: drop each trigger/constraint in a scratch database and see a test fail).
- Error mapping by SQLSTATE only (never by message text); custom codes cannot be spoofed by user data.
- Time semantics (`statement_timestamp()` vs `now()`), lease expiry and `zero_since` with the DB clock; no Rust-clock correctness decisions.
- Isolation: READ COMMITTED reads inside a transaction can see rows committed by others between statements; every read-then-write path must lock first (spec distributed §3d).
- Case sensitivity/collation: lowercase CHECKs and unique indexes must behave identically (`COLLATE "C"`).
- The spike must not weaken or fork any existing behaviour test.

---

### Task PG0: Postgres dev/test infrastructure

**Files:**
- Create: `deploy/dev/postgres.compose.yaml`, `scripts/pg-up.sh`, `scripts/pg-down.sh` (WSL-side helpers, run via `scripts/wsl-cargo.sh`-style wrappers or `wsl.exe`), `crates/rusty-store/src/pg.rs` (behind feature `postgres`: `PgStore::connect(url) -> Result<PgStore>`, `pool()`, `begin_write()` = BEGIN + mailbox-row lock helper later), `crates/rusty-store/tests/common/pg.rs` (isolated-database guard), `crates/rusty-store/tests/pg_smoke.rs`
- Modify: `Cargo.toml`/`crates/rusty-store/Cargo.toml` (sqlx `postgres` feature, cargo features), `deploy/images.lock`, `scripts/ci.sh` (stage `postgres`)

**Interfaces:**
- Produces: `PgStore` (connect with a connect-options builder: TLS mode parameter, statement/idle-in-transaction timeouts, application_name), test helper `pg_env() -> Option<PgTestEnv>` returning an isolated database URL and dropping it on drop.

- [ ] **Step 1: Write failing tests** (`pg_smoke.rs`): `pg_reports_version_18`, `isolated_databases_do_not_see_each_other`, `statement_timeout_is_applied`, `connect_with_wrong_password_is_storage_error_without_leaking_url`, `skipped_loudly_without_env`.
- [ ] **Step 2: Run to confirm FAIL.**
- [ ] **Step 3: Implement** compose file and scripts (digest-pinned image; healthcheck `pg_isready`; `scripts/pg-up.sh` prints the URL to a file under `$HOME` with 0600 and never to stdout logs), `PgStore`, the guard, and the `ci.sh` stage (starts and stops the container, FAILS if the tests were skipped while Docker works).
- [ ] **Step 4: Run the smoke tests 3 times, `cargo test --workspace` (no Postgres) and clippy; commit** `feat(store): Postgres test infrastructure`.

### Task PG1: Postgres schema, translation rules and parity tests

**Files:**
- Create: `crates/rusty-store/migrations_pg/0001_init.sql` (consolidated port of SQLite migrations 0001-0007, see rules), `crates/rusty-store/tests/pg_schema_constraints.rs` (port of the SQLite negative tests), `crates/rusty-store/tests/pg_schema_parity.rs`, `crates/rusty-store/tests/snapshots/pg_catalog.snap`
- Modify: SQLite migration `0008_list_member_seq.sql` (adds `list_members.seq` ordering column so list expansion no longer depends on SQLite `rowid`; Postgres schema gets the same column), `crates/rusty-store/src/pg.rs` (migrate on connect via `sqlx::migrate!("./migrations_pg")`)

**Translation rules (mandatory, keep a table in the migration header):**
| SQLite | Postgres |
|---|---|
| `INTEGER PRIMARY KEY` ids | `bigint GENERATED ALWAYS AS IDENTITY` (ids are never reused and never exposed globally in public APIs) |
| `STRICT` | native column types; `bytea` with `CHECK (length(...) = 32)` for hashes/ids |
| `json_valid(x)` CHECK on TEXT | `jsonb` column |
| `GLOB` patterns in CHECKs | anchored regex `~` equivalents (list them, with tests of accepted/rejected samples for each) |
| `lower(x) = x` CHECKs | same with `COLLATE "C"` columns |
| `unixepoch()` / `strftime` | `extract(epoch from statement_timestamp())::bigint` (a SQL function `rs_now()` used everywhere) |
| `RAISE(ABORT,'conflict:...')` / `RAISE(ABORT,...)` | `RAISE EXCEPTION ... USING ERRCODE = 'RS001'` (Conflict) / `'RS002'` (Invalid); fixed messages |
| triggers (`AFTER`/`BEFORE`, recursion for folder cycles, refcount/`zero_since`, address uniqueness across tables, capabilities) | plpgsql functions + triggers with identical semantics; folder-cycle walk via `WITH RECURSIVE` bounded by depth 64 |
| `ON DELETE NO ACTION` | `NO ACTION` (checked at statement end, same as SQLite); RESTRICT stays RESTRICT |
| `rowid` ordering of list members | `list_members.seq` |
| `BEGIN IMMEDIATE` | `BEGIN` + `SELECT ... FOR UPDATE` on the mailbox row first |
| `INSERT ... ON CONFLICT DO NOTHING`, `UPDATE ... RETURNING`, recursive CTEs, partial and expression unique indexes | unchanged |

- [ ] **Step 1: Write failing tests:** port EVERY test of `schema_constraints.rs`, `schema_capabilities.rs`, the jobs/deliveries schema tests to `pg_schema_constraints.rs` (same names with a `pg_` prefix, same assertions about which operation fails and the mapped `Error` kind); `pg_schema_parity` compares a normalized catalog dump (tables, columns with normalized types, nullability, defaults, constraints, indexes, triggers by name) of the Postgres schema against the SQLite `sqlite_master` snapshot through an explicit mapping table and fails on any table/column/trigger/index present on one side only; plus Postgres-specific tests: `custom_sqlstate_codes_are_mapped` (RS001 -> Conflict, RS002 -> Invalid, spoof attempt: user data containing 'RS001' text does not change the mapping), `rs_now_uses_statement_timestamp`.
- [ ] **Step 2: Run to confirm FAIL** (no Postgres schema yet).
- [ ] **Step 3: Implement** the migration and the mapping in `map_db_error` (extend by SQLSTATE; the SQLite branch is untouched). Add the `0008` SQLite migration and update the SQLite schema snapshot; make `Directory::resolve` order list members by `seq` on SQLite too (all existing tests stay green).
- [ ] **Step 4: Mutation check** (scratch database): for each trigger and each CHECK/unique index of the Postgres schema drop it and confirm at least one negative test fails; record the table in the report (any survivor means a missing test).
- [ ] **Step 5: Run everything; commit** `feat(store): Postgres schema with parity tests`.

### Task PG2: Vertical-slice spike and backend-abstraction recommendation

**Purpose:** decide the repository-trait boundary with evidence before touching the other ~5,000 lines. Time-box: one slice only.

**Files:**
- Create: `crates/rusty-store/src/repo/mod.rs` (spike traits), `crates/rusty-store/src/repo/sqlite.rs`, `crates/rusty-store/src/repo/pg.rs`, `crates/rusty-store/tests/spike_conformance.rs`, `docs/superpowers/specs/2026-11-03-rusty-store-repo-traits-recommendation.md` (the report)
- Modify: minimal call sites only (new code paths may be exercised by new tests; existing public APIs keep working)

**Slice:** `Directory::create_domain`, `create_account` (transaction incl. mailbox row + system folders), `resolve`; `Mailbox::create_folder`, `append` (blob put + transaction), `list_folder`, `changes_since`. Backend-neutral: validation, normalization, blob IO, MIME parsing. Backend-specific: SQL in `repo::{sqlite,pg}`.

- [ ] **Step 1: Write failing tests** `spike_conformance.rs`: ONE test body per behaviour, run against both backends via a `for_each_backend` helper: create domain/account (duplicate -> Conflict, bad domain -> Invalid), system folders 1..=5, resolve (account/alias unknown -> NotFound), append returns increasing ids/uids, `list_folder` paging, `changes_since` contiguous seq, cross-mailbox ids are NotFound, concurrent appends (50 tasks, pool of 4, multi-thread runtime) yield gapless uid and seq on BOTH backends, a forced blob-order deadlock scenario on Postgres is retried and succeeds.
- [ ] **Step 2: Run to confirm FAIL.**
- [ ] **Step 3: Implement** the slice behind traits chosen by the implementer (record alternatives considered: trait objects vs generics, transaction handle type, how errors are mapped, how batch counters are allocated, how backend-specific SQL is organised) with the smallest API that serves the slice.
- [ ] **Step 4: Measure and write the recommendation report:** lines of code per backend for the slice, list of every dialect difference met, concurrency/deadlock findings, compile-time and test-time impact, how the slice scales to `deliver_many`, `jobs::claim` (FOR UPDATE SKIP LOCKED), GC, capabilities; a go/no-go and a concrete trait list for Part 2, and an estimate of the remaining work in terms of modules and tests (no calendar estimates).
- [ ] **Step 5: Run all tests on SQLite and Postgres 3 times; commit** `feat(store): repository-trait spike with conformance tests` and the report separately (`docs: repository trait recommendation`).

---

## Part 2 (to be written after the PG2 report is reviewed)

Outline only, so nothing here is executed before it is expanded into tasks with exact interfaces:
- PG3 Directory repository extraction (domains, accounts, aliases, lists, config, auth hashes, capabilities) with conformance tests.
- PG4 Mailbox repository extraction (folders, messages, keywords, batch operations, deliveries) incl. blob-ordering and deadlock retry.
- PG5 Jobs (claim with `FOR UPDATE SKIP LOCKED`, fence/complete_in with `FOR SHARE`/`UPDATE`) and GC/purge repositories; `zero_since` DB-clock semantics.
- PG6 Whole-suite parametrisation: every behavioural test runs on both backends; acceptance test (10,000 messages) on Postgres; schema/catalog parity kept green.
- PG7 Benchmark G1/G2 on this machine (SQLite vs Postgres 18; 1/4/16 writers; small and 1 MiB messages; p50/p99; handshake not included), results stored in the repo.
- PG8 Hardening: separate migrator/runtime/read-only roles, `statement_timeout`/`idle_in_transaction_session_timeout` per role, TLS verify-full option in `PgStore::connect`, optional Row-Level Security by `mailbox_id`, WAL archiving notes; Valkey cache comes in its own plan.
