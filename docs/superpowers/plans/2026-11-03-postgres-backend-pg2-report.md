# PG2 report: vertical-slice spike and backend-abstraction recommendation

Status: spike done on branch `main` (commits `ae75541`, `630f321`, `5b10e60`), nothing pushed. The schema (`migrations_pg/0001_init.sql`) needed NO change. Existing `Mailbox`, `Directory`, `JobQueue`, GC and all their tests are untouched (only 8 private helpers in `mailbox.rs` became `pub(crate)`).

Verdict: **GO** with the architecture below. Conditions are listed in section 9.

## 1. What was built

`crates/rusty-store/src/repo/` (new, `pub mod repo`):

| File | Role |
|---|---|
| `mod.rs` | `DirectoryRepo` / `MailboxRepo` traits, neutral DTOs (`NewAccount`, `NewMessage`, `Delivery`, `JobFence`, `Resolution`), shared statement text (`sql_text`), pure policy (`depth_verdict`, `thread_candidates`), facades `SpikeDirectory` / `SpikeMailbox`, one generic read (`changes_since_generic`) |
| `sqlite.rs` | `SqliteRepo` (BEGIN IMMEDIATE + mailbox row first, as `Mailbox::begin` does) |
| `pg.rs` | `PgRepo`: `tx_retry!` macro, `finish`, retry stats, `lock_blobs`, batch insert via `unnest`, test knobs |

Slice (as planned): `create_domain`, `create_account` (mailbox row + 5 system folders), `resolve` (account / alias / list expansion), `create_folder` (depth check, uid_validity, change log), `append` and `deliver` (idempotent by `DeliveryId`), `append_many`, `list_folder`, `changes_since`; plus one thing the plan did not list but the codebase forced (section 3): `append(.., fence: Option<JobFence>)` which fences and completes a claimed job in the same transaction.

Tests (all run on SQLite always, on Postgres when `RUSTY_TEST_PG_URL` is set; added to the `ci.sh` postgres stage):

* `spike_conformance.rs`: 15 tests, ONE body per behaviour through `for_each_backend`. Covers: duplicate domain/account -> Conflict, bad domain -> Invalid (facade and, with the facade bypassed, the DB CHECK), unknown domain FK -> Conflict, system folders 1..=5 and custom from 6, depth limit 64, resolve account/alias/list (member order by `seq`) / unknown -> NotFound, increasing ids/uids, read-back, paging, contiguous `changes_since`, cross-mailbox ids NotFound, `deliver` idempotency (Duplicate returns the ORIGINAL ref, nothing written; same id + other content -> Conflict), threading incl. a reply inside one batch, `append_many` consecutive + all-or-nothing, 50 concurrent appends on a pool of 4 (gapless uid/id/seq), 12 concurrent batches stay consecutive per batch, 20 concurrent deliveries with one `DeliveryId` -> exactly 1 Created, a concurrent reader never sees a hole in `changes_since`, job fence (stale epoch rolls the delivery back; Duplicate path still completes the job), and a tripwire test for `$N` placeholders on SQLite.
* `spike_pg.rs` (Postgres only): forced blob-order deadlock is retried and succeeds (control: 1 real 40P01, 1 retry, 1.6 s because of `deadlock_timeout`); the ordered path ran 40 crossed rounds with 0 deadlocks / 0 retries; 12 mailboxes x 5 batches of the same 3 blobs in rotated and reversed orders: 0 deadlocks and `refcount` exactly 60; real 40001 and 40P01 classified from server errors; a GC-style `DELETE FROM blobs` in flight while a new reference to the same content is stored (the append waits, then succeeds, refcount 1).
* `spike_measure.rs`: `#[ignore]`d measurements (section 5).

Results: workspace on SQLite 393 passed / 0 failed; Postgres suites (`spike_conformance`, `spike_pg`, `pg_schema_*`, `pg_smoke`) green 3 consecutive runs; clippy `-D warnings` clean with and without `--features postgres`. Full `scripts/wsl-ci.sh`: see the final message of the task (run last).

Mutation checks of the spike itself: removing `FOR UPDATE` from the mailbox lock is caught by `concurrent_same_delivery_id_creates_one_message` (and only that one, see 6.3); `MAX_ATTEMPTS = 1` is caught by the forced-deadlock test. The ordering rule is guarded by the negative control plus `ordered_blob_locks_never_deadlock`, not by a mutation run (I did not mutate `lock_blobs` ordering).

## 2. Recommendation: how to abstract the store

### 2.1 Boundary: repositories at OPERATION granularity, transaction owned by the repository

Each repository method is one unit of work: plain data in, plain data out, it begins, locks, runs and commits its own transaction. No `Tx` handle crosses the trait. Reasons, all observed in the spike:

1. **Retry needs whole-operation re-execution.** Postgres 40P01/40001 aborts the transaction; the only safe response is to run the whole body again. That is only possible if the body is a function of its (borrowed, immutable) inputs and nothing outside the repository saw a half-finished attempt. A handle API (`begin()` ... `commit()` driven by the facade) makes every caller write its own retry loop.
2. **Backend-specific tactics stay inside.** `append_many` is 9 statements on Postgres (arrays, one `UPDATE` allocating the id AND seq ranges, one per folder for uids) and 52 / 502 statements for 10 / 100 messages on SQLite (loop). The facade neither knows nor cares.
3. **One place for session policy.** `SET LOCAL rusty.mailbox_id` (RLS, spec 3.2), `lock_timeout`, statement tagging go into `begin_locked` once (PG8).
4. **It lets `pool()` leave the public API** (spec 3d asks for exactly this). In the repo today `pool()` / `begin_write()` / `SqliteTx` are used by `rusty-directory` (11 `pool()` and 7 `begin_write()` call sites), `gc.rs`, `jobs.rs`, `mailbox.rs` and 27 places in `tests/jobs.rs`.

### 2.2 Dispatch: `Arc<dyn Trait>` (async_trait). Alternatives considered

| Option | Verdict |
|---|---|
| **`Arc<dyn MailboxRepo>` / `dyn DirectoryRepo`** | **Chosen.** Facades (`Mailbox`, `Directory`, `JobQueue`) stay non-generic and `Clone`, so no consumer crate or daemon type grows a type parameter. The backend is a runtime choice (config: SQLite for dev/small, Postgres for production), which needs dynamic dispatch or an enum anyway. Cost: one boxed future per operation (not measured; an allocation next to 1.7-3.7 ms operations, treat as noise). |
| Generics `Mailbox<R: MailboxRepo>` | Rejected: infects every user (`Directory<R>`, `JobQueue<R>`, GC, daemons) and still needs an enum or dyn at the top to choose at runtime. Only wins if the backend were a compile-time decision (it is not: both are features of one binary). |
| `enum Repo { Sqlite(..), Pg(..) }` with a `match` per method | Equivalent to dyn minus the allocation; costs a 2-arm match per operation (about 50 operations) and `#[cfg]` variants. Keep as the fallback if dyn ever bites (e.g. a method needing generics, which is not object safe). |
| `sqlx::Any` | Rejected, not tried: loses type information (no `bytea`/`bigint[]`, no arrays), and the whole point is backend-specific SQL where the dialects differ (sections 4, 6). |
| Transaction handle trait (`Tx` as in the existing `traits.rs`) | Rejected as the primary boundary (2.1). The current `traits.rs` (`Tx`, `MetadataStore`, `SqliteTx`) is a stub nobody uses and should be deleted in PG3. |

### 2.3 The sync-vs-async question

The premise does not hold in this codebase: SQLite is accessed through **sqlx** (async API, worker thread inside sqlx), exactly like Postgres. There is no sync SQLite code to bridge. Both backends implement the same `#[async_trait]` signatures and compile into the same facades; the spike confirms it (the SQLite repo is plain sqlx code, the only `tokio::spawn` is the existing cancellation-safe `begin_write`). The genuinely blocking parts are blob file IO (already `spawn_blocking` in `FsBlobStore`) and Argon2 (already off-thread in `rusty-directory`). If SQLite were ever replaced by a synchronous driver (rusqlite), operation-granularity is also the right boundary for that: one `spawn_blocking` per repository call, which a handle API could not do.

### 2.4 What the facades keep, and what is shared vs duplicated

Facades keep (and must stay backend-free): validation, size limits, MIME parsing, `blob.put` BEFORE the metadata transaction (a retry never repeats IO), paging limits, result shaping, the DeliveryId contract, the lock-ordering documentation.

Shared (one copy, `repo/mod.rs`, about 50 code lines of SQL plus 180 of facade):

* ~22 statements written once with `$N` placeholders and a `{NOW}` clock token (`unixepoch()` vs `rs_now()`): folder lookup, resolve, list members, ancestor CTE, folder insert, change_log insert, uid and id/seq allocation, delivery lookup, list_folder, changes, job completion. **`$N` works unchanged on SQLite through sqlx** (`$2 - $1` with binds 10, 3 gives -7; guarded by a tripwire test). I had expected to need a `?N` -> `$N` rewriter and deleted it.
* Pure policy: `depth_verdict` (the database only returns rows), thread-candidate selection, `to_uid` / `to_flags`, `page_limit`.
* Row-to-DTO mapping can be shared with a generic function over `sqlx::Database`: `changes_since_generic` compiles with 5 bound lines and is used by both backends. I did NOT generalise further; for anything that binds non-primitive types or has per-backend decode (bytea vs hex text) the bound list grows and the macro/duplication cost is lower. Rule of thumb for Part 2: generic only for read queries with primitive columns.

Duplicated per backend (about 450 lines SQLite, 480 lines Postgres operations + 160 Postgres infrastructure, spike scale):

* bind/decode glue (hash: 64-char hex `TEXT` on SQLite vs 32-byte `bytea` on Postgres via `hash_bytes`; `deliveries.blob_hash` decodes to `String` vs `Vec<u8>`);
* transaction orchestration and locking (BEGIN IMMEDIATE vs `FOR UPDATE`; `begin_locked`);
* batch strategy (arrays/`unnest`/`ANY` vs loops/dynamic `IN (...)`);
* statements with a different shape (uid_validity `max()` vs `GREATEST()`, account creation).

## 3. Cross-module atomicity: `JobFence` instead of a handle

The one real argument for exposing a transaction is `JobQueue::complete_in(tx: &mut SqliteTx, ..)` and `fence`, used so a job's effect and its completion commit together. In the spike this is a parameter: `append(.., fence: Option<JobFence>)` runs `UPDATE jobs SET state='done' .. WHERE id=$1 AND epoch=$2 AND state='running' AND lease_expires_at > {NOW}` in the same transaction; 0 rows -> `Conflict("stale job epoch")` and everything rolls back (tested on both backends, including the Duplicate path). Part 2 rule: every repository operation a job step may perform atomically takes `Option<JobFence>`. Today that is `deliver`, `deliver_many` and, in practice, whatever the worker kinds do (to be listed in PG5; the codebase only has the `jobs` tests as callers). If a case appears where a fence cannot be a parameter, the fallback is a closed `enum Tx { Sqlite(SqliteTx), Pg(PgTx) }` with `*_in(&mut Tx, ..)` methods; I did not need it and did not build it.

## 4. Dialect differences actually met

1. Clock: `unixepoch()` vs `rs_now()` (`{NOW}` token). The spike uses the DB clock on both backends for `created_at`, `internal_date` and lease checks; the existing SQLite facades use the Rust clock for `internal_date` / `created_at`. Part 2 must decide per column and keep the `_at(now)` twins (spike did not need them).
2. Hash representation: hex `TEXT` vs `bytea(32)`; deliveries likewise.
3. Integer literals: a bare `1` in a `UNION ALL` is `int4` on Postgres and the `i64` decoder fails; needs `CAST(1 AS BIGINT)`. Derived tables need an alias on Postgres (`) AS u`).
4. Arrays: Postgres `= ANY($2)`, `unnest(...)`; SQLite has none (dynamic placeholders). This is the main reason batch code differs.
5. Scalar `max(a,b)` / `min` vs `GREATEST` / `LEAST`.
6. `ON CONFLICT DO NOTHING` on `blobs` does not lock the existing row on Postgres; a lock must be added (`SELECT .. FOR NO KEY UPDATE`, section 6.2). It does not work at all on `accounts`/`aliases`/`lists` (the registry trigger raises RS001 first; schema header already says so).
7. Duplicate address: SQLite trigger message `conflict:...` (code 1811) vs Postgres SQLSTATE `RS001`; both map to `Conflict`. Duplicate folder name, domain: plain UNIQUE (23505 / SQLite UNIQUE) -> `Conflict`. Bad local part / domain: CHECK -> `Invalid` on both. Unknown domain: FK -> `Conflict` on both. All verified by the conformance tests.
8. Locks: SQLite takes one global write lock at BEGIN; Postgres locks rows (mailbox, blobs, folders). Consequences in section 5/6.
9. Statement counts per operation (sqlx query events, includes BEGIN/COMMIT): append 7 (SQLite) vs 9 (Postgres); `append_many(10)` 52 vs 9; `append_many(100)` 502 vs 9; `create_account` 7 vs 6; `create_folder` 6 vs 7; `list_folder` 2 and `changes_since` 1 on both. Postgres round trips matter, SQLite statements are in-process: do not optimise the SQLite batch path, do optimise Postgres statement count (6.4).
10. Pool semantics: see head-of-line blocking (5.3).

## 5. Measured behaviour

Setup: this machine, WSL2; Postgres 18 from the test container (tmpfs, `fsync=off`, loopback through Docker's port proxy), SQLite a file database in a temp dir with sqlx defaults; pool of 4 on both; small messages (~150 bytes). **Absolute numbers are not production numbers** (single run each, shared machine with another agent building). Only the shape is meaningful.

### 5.1 Throughput and latency of `append`

| Workload | SQLite ops/s (p50 / p99) | Postgres ops/s (p50 / p99) |
|---|---|---|
| 1 mailbox, 1 writer | 561 (1.7 / 2.6 ms) | 260 (3.7 / 4.9 ms) |
| 1 mailbox, 4 writers | 638 (2.8 / 55 ms) | 360 (7.9 / 44 ms) |
| 1 mailbox, 16 writers | 545 (19.7 / 73 ms) | 379 (41 / 53 ms) |
| 8 mailboxes, 8 writers | 576 (8.1 / 110 ms) | **858** (9.1 / 11 ms) |
| 8 mailboxes, same blob | 534 (8.2 / 187 ms) | 526 (14.6 / 18 ms) |
| 1 mailbox, same blob, 8 writers | 613 (8.1 / 137 ms) | 414 (19 / 25 ms) |
| 200 single appends vs 2 x `append_many(100)` | 337 ms vs 148 ms | **803 ms vs 67 ms** |

Reading it: SQLite wins single-writer latency (no round trips) and is flat across mailbox counts (one global writer). Postgres is slower per call but is the only one that scales with independent mailboxes (858 vs 576 ops/s at 8 mailboxes, and the p99 is 10x tighter), which is the spec's claim (§3.1: "khóa đúng một hàng mailbox"). One mailbox serializes on its row at about 1/(transaction time), ~260-380 appends/s here; that is fine for deliveries (scale target 1M mails/day is ~12/s average) but bulk import of one mailbox must use `append_many` (12x faster than singles on Postgres, 2.3x on SQLite). Delivering one popular blob to 8 mailboxes costs 40% of throughput versus distinct blobs on Postgres (hot `blobs` row held until commit): expected, no deadlock, see 6.2; mitigation is only needed if a mailing-list fan-out hits it (PG7 to measure).

### 5.2 Gaplessness under real concurrency (Review Focus 1)

50 concurrent appends on a pool of 4: uids 1..=50, ids 1..=50 and change seqs 1..=50, no gaps, on both backends. 12 concurrent batches of 5: ids 1..=60 and every batch internally consecutive. A concurrent reader polling `changes_since` while 4 writers append never observed `seq` skipping (the mailbox lock is held from allocation to commit, so seq N+1 cannot become visible before N). 20 concurrent deliveries with one `DeliveryId`: exactly one Created, 19 Duplicate, one message. Not tested: gaplessness when a writer crashes mid-transaction (rollback returns the counter bump, by transactional construction; no test), and counters for `folder_id` / `uid_validity` under concurrency beyond what `create_folder` conformance covers.

### 5.3 Head-of-line blocking through the pool (new finding)

A hot mailbox with 12 writers; latency of one append into an unrelated cold mailbox:

| | cold-mailbox append p50 / p99 |
|---|---|
| SQLite, idle | 1.6 / 3.4 ms |
| SQLite, 12 hot writers | 16.5 / 35.8 ms |
| Postgres pool 4, idle | 3.9 / 9.8 ms |
| Postgres pool 4, 12 hot writers | **25.2 / 29.6 ms** |
| Postgres pool 16, 12 hot writers | 4.3 / 4.9 ms |

Every transaction waiting on the hot mailbox's row lock holds a pooled connection, so with a small pool the cold mailbox queues in `acquire` behind them. Row locks give no isolation if the pool is smaller than the number of concurrent writers of the hottest mailbox. Consequences for Part 2/PG8: size the pool above the expected hot-mailbox concurrency; add an in-process per-mailbox async queue (a `tokio` semaphore keyed by mailbox id, taken BEFORE acquiring a connection) so waiters do not hold connections; set `lock_timeout` (so a lock held by a stuck holder fails fast with 55P03 instead of waiting out `statement_timeout`); map pool-acquire timeout to a transient error (section 7). The per-process queue only helps per `store` process; with several store nodes the Postgres lock is still the arbiter (correct, just slower).

### 5.4 Build and test cost (dev profile, cold private target dir, noisy machine)

* Cold build of `rusty-store` with dependencies: 18 s SQLite only; building the Postgres feature on top adds about 15 s (sqlx-postgres and friends, one time).
* Rebuild of the `rusty-store` library alone after `cargo clean -p`: 4 s with the spike vs 3 s at baseline `067fd8d` (1 s resolution), same for `--features postgres`. The spike adds 1,731 lines of source (`repo/` mod 555, sqlite 461, pg 715) and 1,193 lines of tests and harness.
* Test time: `spike_conformance` 0.37 s on SQLite only, 1.2-1.3 s with Postgres (each test creates and drops its own database); `spike_pg` 1.9 s (dominated by one `deadlock_timeout` of 1 s).
* The CI cost is the Postgres container start and the second compile of the test binaries with the feature, both already paid by PG0.

## 6. Transaction / retry helper design (as built)

### 6.1 Shape

```text
tx_retry!(self, Some(mailbox_id), |c| { body })   // c: &mut PgConnection, body: TxResult<T>
  loop attempt 0..MAX_ATTEMPTS(5):
    tx = begin (READ COMMITTED) ; SELECT id FROM mailboxes WHERE id=$1 FOR UPDATE  (NotFound if absent)
    res = body(c).await
    finish(tx, res, attempt):
      Ok            -> COMMIT ; commit error is classified like body errors
      Err(TxError)  -> ROLLBACK (result ignored) ; classify:
          Db(40P01|40001)  -> stats.retries++ (deadlocks++ for 40P01) ; jittered backoff 5ms*2^n(+jitter) ; Retry
                              after the last attempt: Storage("transaction retry limit exceeded"), stats.exhausted++
          Db(other)        -> map_db_error (SQLSTATE only)
          Domain(Error)    -> returned as is
```

`TxError::{Db(sqlx::Error), Domain(Error)}` exists so the SQLSTATE is still visible to the loop (`map_db_error` flattens it); `?` converts both. Rules: retry ONLY on 40P01/40001 (the transaction is certainly aborted); never on connection errors or a failed COMMIT with unknown outcome (the commit may have happened; a non-idempotent `append` retried there would duplicate; `deliver` is safe because of the `DeliveryId`). The body must be re-runnable: no side effects outside the transaction; `blob.put` happens before the call.

### 6.2 Why a macro and not a closure-taking method

The first version was `async fn run(&self, lock, op: impl for<'c> FnMut(&'c mut PgConnection) -> BoxFuture<'c, T>)`. rustc rejects it as soon as the closure borrows an argument of the surrounding `async_trait` method ("returning this value requires that `'life1` must outlive `'static`"): the higher-ranked connection lifetime cannot be combined with captured non-`'static` references, and async closures (`AsyncFnMut`) give no stable way to require the returned future to be `Send`. Options, in order of preference: (a) the `tx_retry!` macro (what the spike uses; the loop is inlined so all borrows live in one future; 4 call sites in the spike, trivial to read); (b) a small `#[async_trait] trait Work<T> { async fn exec(&self, c: &mut PgConnection) -> TxResult<T>; }` with one struct per operation (more boilerplate, no macro); (c) clone/`Arc` the inputs into a `'static` closure (extra allocation per attempt). Part 2 can stay with (a); the unit of the macro is only `begin_locked` + `finish`, which are ordinary methods and unit-testable.

### 6.3 Locking rules as implemented

1. First statement of every mailbox write: `SELECT id FROM mailboxes WHERE id=$1 FOR UPDATE`. Observed value: the counters are already serialized by their own `UPDATE ... RETURNING` row locks, so gaplessness does NOT depend on this statement (removing it did not fail any counter test); what depends on it is every read-then-write sequence: the `deliveries` lookup before insert (caught by the 20-way test in `spike_conformance`; `spike_pg` was not run under that mutation), and in Part 2 keyword counting, folder-delete emptiness checks and the cycle trigger. Cost: one extra statement per write; it can be dropped on paths with no read-then-write (plain `append`) if the first `UPDATE mailboxes` is made the first statement. Not done.
2. Blobs: `lock_blobs` inserts the missing rows (array insert in ascending hash order), then `SELECT hash FROM blobs WHERE hash = ANY($1) ORDER BY hash FOR NO KEY UPDATE`, all BEFORE the first message insert, so the refcount trigger finds every row already locked by us. Verified: the control (unordered + pause) deadlocks for real; the ordered path never does. The `FOR NO KEY UPDATE` also closes a GC race: a concurrently deleted unreferenced row is detected by the row count (bounded 3-round loop) and re-created. The interleaving "delete in flight, new reference arrives" is tested; the interleaving "delete commits between my insert and my lock" is covered by the loop but NOT tested (needs a pause between two statements of one transaction).
3. Deadlocks need two different mailboxes. Two writers of the same mailbox are serialized by the mailbox lock before they ever touch `blobs`. A batch of one mailbox cannot deadlock with itself either.
4. Batch counter allocation: ONE `UPDATE mailboxes SET next_msg_id = next_msg_id + n, next_change_seq = next_change_seq + n .. RETURNING next_msg_id - n, next_change_seq - n` (a created message always consumes one of each; the same text serves single and batch and runs on SQLite too), plus one `UPDATE folders SET next_uid = next_uid + k` per distinct folder in ascending order. Rows go in with one `INSERT .. SELECT FROM unnest(..)` per table. This reproduces the sequential result exactly (the same assertions on thread ids, uids and seqs pass on the loop-based SQLite implementation and the batch Postgres one).

### 6.4 Statement count

A Postgres `append` is 9 round trips (BEGIN, mailbox lock, blob insert, blob lock, id/seq, uid, message insert, change insert, COMMIT) against 7 in-process statements on SQLite; p50 3.7 ms vs 1.7 ms. First optimisations when PG7 measures: merge the two blob statements into one CTE, merge the mailbox lock into the first `UPDATE` for non-read-then-write paths, merge the message and change inserts into one CTE. Expect 9 -> 5. Not done in the spike (correctness first); sqlx has no pipelining.

## 7. Error mapping

* One function stays: `map_db_error(sqlx::Error) -> Error`, by SQLSTATE and sqlx `ErrorKind` only (never message text for Postgres): `RS001` -> Conflict, `RS002` -> Invalid, 23505 / 23503 -> Conflict, 23514 / 23502 -> Invalid, SQLite trigger code 1811 keeps its `conflict:` prefix rule. Already implemented by PG1 and exercised on both backends by the conformance tests (duplicate domain, account, folder name; unknown domain; bad domain and local part).
* Add to it in PG3 (the retry loop reads the raw `sqlx::Error` first, so `map_db_error` only ever sees what is final): `40P01` / `40001` after exhaustion, `55P03` (lock_timeout), `57014` (statement_timeout / cancel), `53300` (too many connections), pool-acquire timeout and `PoolClosed` -> a **transient** error class. `rusty_core::Error` has no such variant today (`Storage` is used, which a daemon cannot distinguish from corruption); I recommend a new `Error::Busy(String)` (fixed text) so the SMTP side can answer 451 and the API 503. That is a change to `rusty-core` and needs the lead's decision; the spike left everything as `Storage` with fixed strings.
* The RS003 registry guard (`address_owners is maintained by triggers only`) can only fire from a bug or a hostile role, never from a repository operation; it falls into the "unmapped SQLSTATE -> Storage" branch on purpose (verify in PG3 that it is not accidentally mapped to Conflict).
* `begin_write` currently turns every begin failure (including pool timeout) into `Storage("cannot begin Postgres transaction")`; it needs the transient mapping above.

## 8. What hurts

1. **Retry + borrowed async closures** (6.2). Solved with a macro; keep it small.
2. **No compile-time dialect checking.** All SQL is runtime strings; a Postgres typo shows up only when a test runs it on Postgres. I wrote the known traps (`int4` literal needing a cast, derived-table alias) correctly up front, and the conformance suite passed on Postgres on its first run, so this report has NO evidence of the suite catching a dialect slip; PG6's mutation run should include dialect mutations. The Postgres stage of CI must stay mandatory (it is: `ci.sh` fails when Docker works but the tests did not run) and every repository method needs a conformance test on both backends, with no exceptions.
3. **Two sets of glue for the same statement** (bind/decode differences). The shared-text trick removes the SQL duplication but not the Rust duplication (about 150 lines per backend in the slice).
4. **Edition-2021 disjoint closure capture** bit the test harness: `async move { b.dir ... }` captures only `b.dir` and drops `b._tmp` (the temp dir, hence "attempt to write a readonly database" on SQLite). Fixed with a `Drop` impl on `Backend` (a type with `Drop` is captured whole). Worth knowing for every `for_each_backend` user.
5. **Pool head-of-line blocking** (5.3) is a new failure mode that SQLite does not have and that unit tests do not show.
6. **Round trips** (6.4): Postgres cost per operation is dominated by statement count, so statements must be counted and budgeted (the existing `query_shape.rs` oracle style needs a Postgres twin with different numbers).
7. **Hot `blobs` rows** (5.1, popular message to many mailboxes) serialize at commit; accepted, to be measured at fan-out sizes in PG7.
8. **Time semantics change** when SQLite also uses the DB clock for `internal_date` / `created_at` (different from today's Rust clock): every `_at(now)` twin and every test that injects time has to be audited per column in Part 2.
9. **`jsonb`** config round-trips not byte-identical (review item 12); the spike did not touch config.

## 9. Go / no-go, conditions, and what is NOT verified

**GO**: the slice works on both backends behind one set of traits, with equal observable behaviour on all 15 conformance bodies, and the Postgres-specific hazards the plan worried about (counter gaps, blob-order deadlocks, lost updates in read-then-write, idempotent delivery) are covered by real two-session tests. Conditions for Part 2:

1. The Postgres CI stage stays mandatory and every repository method gets a both-backend conformance test.
2. `Error::Busy` (or an agreed equivalent) before any daemon depends on retry behaviour.
3. Pool sizing and an in-process per-mailbox queue are designed in PG8, not left to defaults (5.3).
4. Decide the clock policy per column (8.8) before PG3 touches `created_at` / `internal_date` / leases.

Not verified (honest list):

* Behaviour on a real network, TLS, a non-tmpfs disk with `fsync=on`, Postgres failover, a lost COMMIT acknowledgement. All numbers come from a loopback tmpfs container with `fsync=off`; the SQLite numbers use sqlx defaults (rollback journal). Do not quote them as a SQLite-vs-Postgres benchmark; that is PG7.
* Larger messages (all spike messages are ~150 bytes), large mailboxes (thousands of rows in `list_folder`), query plans and index use on realistic data; no `EXPLAIN` was looked at.
* `deliver_many`, `jobs::claim` with `FOR UPDATE SKIP LOCKED`, `purge_*`, `collect_blobs`, capabilities, config, soft-delete and keyword paths were NOT ported; the statements about them (section 10) are reasoning from reading the code, not evidence.
* The "delete commits between blob insert and blob lock" GC interleaving (6.2) and `lock_timeout` behaviour.
* The mutation check of the ordering code itself; the retry loop under exhaustion (5 consecutive deadlocks) is untested.
* Dyn dispatch overhead is not measured.
* Crash safety of the counters under `kill -9` mid-transaction (transactional by construction).
* `FOR NO KEY UPDATE` vs `FOR UPDATE` on the mailbox row: the plan says `FOR UPDATE` and the spike follows it; `FOR NO KEY UPDATE` would not block FK key-share checks of child inserts by non-lock-first writers and is a candidate for PG8, unmeasured.
* The facade stand-ins: `SpikeDirectory` validates domain/local-part with a trivial check because IDNA/local-part normalization lives in `rusty-directory` (depends on this crate). Part 2 passes already normalized values into the repository (as the spike repository signature does).

## 10. Scaling the slice to the rest

| Area | How it maps | Postgres specifics | Size |
|---|---|---|---|
| `deliver_many` | `append_many` with a `deliveries` pre-lookup in one `= ANY($2)` query, in-batch duplicate handling, then `insert_messages` for the not-yet-seen items; all-or-nothing semantics from today's code | `lock_blobs` over the whole batch (one ascending lock pass); per-item outcome vector; the 64 MiB / 1000-item bounds stay in the facade | M |
| set_flags / move / expunge (single and `_many`) | read-then-write on `messages` under the mailbox lock; counters for change_log via the same one-statement range allocation (`log_message_changes` becomes `unnest`) | refcount trigger touches `blobs` on move? No (blob unchanged); expunge soft-deletes (no refcount change); `purge_deleted` deletes rows (refcount triggers: needs the ascending-blob rule again) | M |
| keywords | count-before-insert is read-then-write: mailbox lock first (already the rule) | none beyond the rule | S |
| folders (rename/move/delete) | the cycle/depth trigger already takes the mailbox lock itself (PG1); repository takes it first, so lock order is consistent | `delete_folder` deletes messages -> refcount trigger -> blobs: ascending rule | S |
| `jobs::claim` | `UPDATE jobs SET .. WHERE id = (SELECT id FROM jobs WHERE .. ORDER BY run_after, id FOR UPDATE SKIP LOCKED LIMIT 1) RETURNING ..` replaces the select-then-update loop; dead-letter settling stays a loop in the same transaction | lease clock = `{NOW}`; `kind IN (..)` becomes `= ANY($1)`; no mailbox lock (jobs are not mailbox-scoped) so `run(None, ..)` | M |
| `heartbeat` / `complete` / `fail` / `fence` | single-statement fenced updates (the `COMPLETE_JOB` text is already shared); `fence`/`complete_in` become `JobFence` parameters (section 3) | none | S |
| GC (`collect_blobs`, `purge_*`) | batches of ascending hashes (already how the SQLite code walks); delete rows in ascending order | cutoff from the DB clock instead of `SystemTime` (spec 3d); advisory lock instead of `.gc.lock` is a separate spec item; deleting a row whose lock we want waits for in-flight appends, which `lock_blobs` makes safe (tested one interleaving) | M |
| capabilities / auth / config | each is one or two statements under no mailbox lock; `capabilities` insert needs the composite FK; config `jsonb` vs text policy decision | `set_status` + capability revocation transaction: lock account row | M |

## 11. Concrete task list for Part 2 (sizes in modules and tests, no calendar estimates)

Existing surface to cover: about 50 distinct operations (17 mailbox, 21 directory/config/auth/capabilities, 8 jobs, 4 GC), roughly 4,000 lines of SQL-bearing code in `mailbox.rs`, `dir.rs`, `jobs.rs`, `gc.rs`, `caps.rs`, `config.rs`, `auth.rs`; the spike ported 9 of them in ~450 + ~640 lines of repository code. Extrapolation: about 2.5k lines per backend plus shared facades.

* **PG3 Repo foundation + Directory.** Promote `repo/` out of "spike" naming (`Repos { directory, mailbox, jobs, gc }` bundle, `Store::open(config)` selecting the backend); move `tx_retry!`, stats, knobs into `repo/pg/tx.rs`; delete `traits.rs` stub; extend `map_db_error` + add the transient error (section 7); `DirectoryRepo` complete (domains, plans, accounts, aliases, lists, members, resolve, config, status, password hash, capabilities); `Directory` becomes a facade over `Arc<dyn DirectoryRepo>` (stop exposing `store()`/`pool()` to `rusty-directory` callers); promote `common/spike.rs` to the shared harness; port the `rusty-directory` tests to `for_each_backend`. Modules: 3 (repo, directory facade, error mapping); tests: about 60 ported (directory test files) + error-mapping cases.
* **PG4 Mailbox repo.** The remaining 14 mailbox operations, `deliver_many`, batch operations, keyword paths; replace `Mailbox`'s internals with the facade over `MailboxRepo`; per-mailbox in-process queue (5.3) if PG3 pool work did not already add it. Largest task: 1 module (repo mailbox ~1.2k lines per backend), tests: `mailbox.rs`, `mailbox_paging.rs`, `mailbox_props.rs`, `batch.rs`, `deliveries.rs`, `query_shape.rs` (needs a Postgres statement-count twin), `blob_fs.rs` unaffected. Property tests (proptest) must run against both backends with a bounded case count on Postgres.
* **PG5 Jobs + GC.** `JobRepo` with `SKIP LOCKED` claim and `JobFence` parameters on the operations that need it; `GcRepo` with DB-clock cutoffs and ascending-hash deletes; tests `jobs.rs` (27 SqliteTx call sites to convert), `gc.rs`; add the untested GC interleaving test with a pause knob.
* **PG6 Whole-suite parametrisation.** Every behavioural test runs through `for_each_backend` (the test helper `common::Env` currently exposes raw SQLite pools: abstract the few raw fixtures behind helper functions as `exec` in the spike); 10,000-message acceptance test on Postgres; schema/catalog parity kept green; mutation run over the repository code (drop `FOR UPDATE`, drop ordering, drop retry, drop fence) recorded like PG1's table.
* **PG7 Benchmarks.** Promote `spike_measure.rs` to a repeatable harness; real disk, `fsync=on`, larger messages, 1/4/16 writers, fan-out of one blob to 1000 mailboxes, statement-count reductions (6.4), pool size sweep; store results in the repo.
* **PG8 Hardening.** Roles (migrator / runtime / read-only), `lock_timeout` + per-role timeouts, TLS verify-full, RLS via `SET LOCAL rusty.mailbox_id` inside `begin_locked`, no DML/TRUNCATE on `address_owners` for the runtime role (PG1 review item 9), migrator separated from `PgStore::connect` (item 13), pool sizing and queueing, `FOR NO KEY UPDATE` evaluation, WAL archiving notes. Valkey cache invalidation hooks run AFTER the final commit of `tx_retry!` (never inside the retry loop), carrying the committed `seq` returned by the operation.

Suggested order: PG3 -> PG4 -> PG5 -> PG6 (PG7 can start after PG4 for the append path) -> PG8. PG4 is the risk concentration (batch + ordering + property tests); PG3 is mostly mechanical but carries the two decisions that need the lead (`Error::Busy`, clock policy).

## 12. Risks

* Postgres statement latency per operation (2x SQLite single-writer) may matter for interactive IMAP flag storms; mitigated by batch APIs and the CTE merging in 6.4, unproven.
* A repository method that forgets the mailbox lock still passes counter tests (6.3.1); only read-then-write tests catch it. PG6's mutation run is the guard; a lint-like test that every `MailboxRepo` write method starts with the lock (query log inspection via the existing tracing probe) is worth adding.
* Divergence between the two backends' notion of "now" (8.8).
* Pool starvation under a hot mailbox (5.3).
* Cost of keeping two implementations of ~50 operations in lockstep: the conformance harness is the only thing keeping them honest; do not let any operation land without a both-backend test.

## 13. Addendum (PG5 review I1): index on `messages(blob_hash)`

The spike schema had no index on the foreign key column `messages.blob_hash -> blobs.hash ON DELETE RESTRICT`. Postgres runs `SELECT 1 FROM ONLY messages WHERE blob_hash = $1 FOR KEY SHARE` for every `blobs` row a statement deletes (SQLite probes the child table the same way), so without an index each deleted blob row costs a scan of `messages`.

Measured (Postgres 18 container, tmpfs, 101,000 `blobs` rows, 100,000 `messages` rows, one `GC_BATCH` = `DELETE` of 1,000 unreferenced blob rows, simplified two-table schema with the same FK; `psql \timing`, single run each, 2026-11-04):

| | delete of 1,000 blob rows |
|---|---|
| no index on `messages(blob_hash)` | 4,236 ms (review run: 5.2-5.9 s) |
| `CREATE INDEX messages_blob_hash ON messages (blob_hash)` | 12.8 ms (review run: 17 ms) |

The cost without the index grows linearly with `messages` (about a minute per batch at 1M messages) and the batch holds 1,000 blob row locks meanwhile, so under PG8 timeouts GC would degrade to `Busy` forever. The index costs about 58 bytes per message (review measurement; B-tree deduplication helps the one-blob-to-many-mailboxes fan-out) and one index write per append and expunge. It is now in `migrations_pg/0001_init.sql` and SQLite `migrations/0009_messages_blob_hash_index.sql`; the regression tests are `pg_plans::gc_delete_foreign_key_probe_uses_the_blob_hash_index` and `schema_constraints::messages_blob_hash_is_indexed_for_the_gc_foreign_key_probe`.

## 14. Addendum (PG5 review I3): job claim plan

The first PG5 claim statement filtered `(state = 'queued' AND run_after <= now) OR (state = 'running' AND lease_expires_at <= now)` with `ORDER BY run_after, id LIMIT 1 FOR UPDATE SKIP LOCKED`; the disjunction defeated `jobs_state_run_after`, so every claim read, filtered and sorted all job rows. Measured (Postgres 18 container, 502,000 jobs: 300k queued with ties on `run_after`, 2k running with live leases, 200k done; kind filter matching 75%; `EXPLAIN (ANALYZE, TIMING OFF)`, 2026-11-04):

| claim candidate search | execution time | plan |
|---|---|---|
| single disjunction (old) | 69.7 ms | Seq Scan on jobs (277,000 rows removed by filter), external merge sort 7 MB |
| two ordered index probes, window of 32 (new) | 0.08 ms | `jobs_queued_order` + `jobs_running_lease`, Merge Append, quicksort of the tiny expired set |

The claim is now a peek of 32 ids plus a primary-key `FOR UPDATE SKIP LOCKED` statement (5 statements with BEGIN/COMMIT instead of 4; budget raised). Postgres `jobs` drops `jobs_state_run_after` (SQLite keeps it; recorded in `KNOWN_DIFFS`): with ties on `run_after` (a second resolution) its plans need an incremental sort over every tied row. Regression tests: `pg_plans::claim_probes_use_index_scans_at_scale` (202k rows, generic and concrete plans of the production statements), `pg_jobs_gc_concurrency::claim_continues_past_a_window_of_locked_jobs`.

Known limit: the kind filter is not an index column. A worker for a rare kind behind a large backlog of other kinds reads past that backlog (bounded by the queued rows before its first match); a `(kind, run_after, id)` index would fix a single-kind claim and is a PG7 measurement item.
