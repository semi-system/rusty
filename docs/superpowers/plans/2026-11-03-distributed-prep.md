# Distributed-Model Preparation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the four cheap, single-node-compatible building blocks that the distributed model needs later: protocol version negotiation, idempotent delivery, a lease/fencing job queue, and revocable hashed capabilities.

**Architecture:** All four live in the existing crates (no new crate): `rusty-core` (protocol version), `rusty-store` (deliveries, jobs), `rusty-directory` (capabilities). Each adds one migration (next free number after 0003), uses `begin_write` (BEGIN IMMEDIATE), takes time from the database clock (with `*_at(now)` variants for tests), and keeps ids typed.

**Tech Stack:** Rust 1.89+, sqlx (SQLite now; written so the Postgres port can map 1:1), rand_core OsRng + sha2 (already in the tree), serde_json, proptest, tokio.

**Spec:** `docs/superpowers/specs/2026-11-03-rusty-distributed-model-design.md` (§4 delivery, §5 jobs/lease/fencing, §8 versioning, §10 capabilities, §12 prep list). Context: `docs/superpowers/specs/2026-11-02-rusty-foundation-design.md` §8b, and `docs/superpowers/specs/2026-11-03-rusty-postgres-valkey-design.md` §3c (tenant) and §3d.

## Global Constraints

- `#![forbid(unsafe_code)]` stays in every crate; no panics on hostile input; errors are fixed strings that never echo caller input (use the existing `Error` variants).
- Migrations 0001-0003 are frozen; new migrations take the next free numbers, each with negative tests and an update of the committed schema snapshot (`crates/rusty-store/tests/snapshots/schema.sql.snap`, regenerate with `RUSTY_UPDATE_SNAPSHOT=1`).
- Every write transaction uses `begin_write`; every statement that takes an id is scoped by `mailbox_id`/`domain_id` as appropriate; tables stay `STRICT` with the same constraint discipline as before (FK, CHECK, no NULL unless meaningful).
- Time: correctness decisions (lease expiry, token expiry, retention) use the DB clock (`unixepoch()`); public APIs have a `*_at(now: i64)` twin so tests never sleep.
- Concurrency tests use a FILE-backed pool of 4 connections, not the single-connection in-memory pool.
- Hard limits (documented constants): job payload <= 64 KiB, job kind <= 64 bytes `[a-z0-9._-]`, attempts <= 20, claim batch = 1, capabilities per account <= 64, capability TTL <= 24 h.
- Gate: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo deny check`, `bash scripts/ci.sh` all pass.

## Review Focus

- Same `delivery_id` replayed with DIFFERENT content must be a hard `Conflict`, never a silent "duplicate" that drops a different message.
- Two concurrent deliveries with the same `delivery_id` (4 connections) create exactly one message and one change_log entry; the loser gets the original `MessageRef`.
- A worker whose lease expired (zombie) can neither heartbeat, complete, nor pass the fence check after another worker re-claimed the job.
- A job that keeps failing ends in `dead` after the attempt cap, and a poisoned payload cannot crash `claim` (hostile JSON/oversized payload rejected at enqueue).
- Capability tokens: only the hash is stored; wrong/forged/expired/revoked tokens, a locked/disabled account, and a deleted account all validate as "not valid" with identical, constant-shape results (no oracle for which case); revocation on password change and status change.
- Unbounded growth: deliveries, jobs and capabilities all have a retention/purge path and per-account caps.

---

### Task P1: Protocol version negotiation (rusty-core)

**Files:**
- Create: `crates/rusty-core/src/proto.rs`; Modify: `crates/rusty-core/src/lib.rs`
- Test: `crates/rusty-core/tests/proto.rs`

**Interfaces:**
- Produces: `pub const PROTOCOL_VERSION: u16 = 1;` `pub struct VersionRange { pub min: u16, pub max: u16 }` (constructor `VersionRange::new(min, max) -> Result<VersionRange>` rejecting `min > max` or `min == 0`), `pub fn negotiate(ours: VersionRange, peer: VersionRange) -> Result<u16>` returning the highest common version or `Error::Invalid("protocol version mismatch")` when the ranges do not overlap; `pub const SUPPORTED: VersionRange` (currently 1..=1).

- [ ] **Step 1: Write failing tests:** `negotiates_highest_common`, `disjoint_ranges_are_invalid`, `zero_version_rejected`, `min_greater_than_max_rejected`, `negotiation_is_symmetric` (proptest over all u16 pairs of ranges: `negotiate(a,b) == negotiate(b,a)`), `error_message_is_fixed` (does not contain the peer's numbers).
- [ ] **Step 2: Run to confirm FAIL:** `cargo test -p rusty-core --test proto`.
- [ ] **Step 3: Implement** the items above in `proto.rs`.
- [ ] **Step 4: Run to confirm PASS** and commit `feat(core): protocol version negotiation`.

### Task P2: Idempotent delivery (rusty-store)

**Files:**
- Create: `crates/rusty-store/migrations/<next>_deliveries.sql`
- Modify: `crates/rusty-store/src/mailbox.rs`, `crates/rusty-store/src/gc.rs`, `crates/rusty-store/src/lib.rs`, `crates/rusty-core/src/ids.rs`
- Test: `crates/rusty-store/tests/deliveries.rs`, schema snapshot

**Interfaces:**
- Consumes: `Mailbox::append` internals (blob put, parse, transaction).
- Produces:
  - `DeliveryId(pub [u8; 32])` in rusty-core with `DeliveryId::derive(content_hash: &BlobHash, recipient: &str, session: &str) -> DeliveryId` (SHA-256 over length-prefixed fields), `from_hex/to_hex`.
  - Table `deliveries(mailbox_id, delivery_id BLOB(32), message_id, blob_hash, created_at, PRIMARY KEY (mailbox_id, delivery_id), FOREIGN KEY (mailbox_id, message_id) REFERENCES messages ON DELETE CASCADE)`, `STRICT`, `CHECK (length(delivery_id) = 32)`.
  - `Mailbox::deliver(&self, folder: FolderId, raw: Bytes, flags: Flags, delivery_id: DeliveryId) -> Result<DeliverOutcome>` with `enum DeliverOutcome { Created(MessageRef), Duplicate(MessageRef) }`. Same id + same blob hash -> `Duplicate` with the ORIGINAL ref (no new message, no change_log row); same id + different blob hash -> `Error::Conflict("delivery id reused with different content")`. `append` keeps its current non-idempotent behavior.
  - `purge_deliveries(store: &SqliteStore, older_than: i64) -> Result<u64>` (batches of 1000) in gc.rs, retention default constant `DELIVERY_RETENTION_SECS = 7 * 86400`.

- [ ] **Step 1: Write failing tests:** `deliver_creates_then_duplicate_returns_original`, `duplicate_writes_no_second_message_or_change`, `same_id_different_content_is_conflict`, `concurrent_duplicate_deliveries_create_one_message` (50 tasks, same id, file DB pool of 4: exactly one `Created`, 49 `Duplicate`, one message row, one change_log create), `different_mailboxes_same_id_are_independent`, `deliver_to_foreign_folder_is_not_found`, `delivery_row_cascades_with_message_purge`, `purge_deliveries_removes_only_old_rows` (using `purge_deliveries_at`), `quota_and_size_limits_apply_to_deliver`, `delivery_id_derive_is_stable_and_field_separated` (("ab","c") != ("a","bc")), schema negative tests for the new table.
- [ ] **Step 2: Run to confirm FAIL.**
- [ ] **Step 3: Implement** the migration, `DeliveryId`, `deliver` (single write transaction: look up `(mailbox_id, delivery_id)` first; on hit compare `blob_hash`; on miss do the same work as `append` and insert the `deliveries` row), and `purge_deliveries`. Refactor shared code with `append` rather than duplicating it.
- [ ] **Step 4: Run focused tests, then the workspace tests; regenerate the schema snapshot; commit** `feat(store): idempotent delivery with delivery ids`.

### Task P3: Job queue with lease and fencing (rusty-store)

**Files:**
- Create: `crates/rusty-store/migrations/<next>_jobs.sql`, `crates/rusty-store/src/jobs.rs`
- Modify: `crates/rusty-store/src/lib.rs`, `crates/rusty-core/src/ids.rs` (`JobId`)
- Test: `crates/rusty-store/tests/jobs.rs`, schema snapshot

**Interfaces:**
- Produces: table `jobs(id INTEGER PRIMARY KEY, kind TEXT, domain_id INTEGER NULL REFERENCES domains(id), state TEXT CHECK IN ('queued','running','done','failed','dead'), payload TEXT CHECK (json_valid AND length <= 65536), attempts INTEGER, max_attempts INTEGER, run_after INTEGER, lease_owner TEXT NULL, lease_expires_at INTEGER NULL, epoch INTEGER NOT NULL DEFAULT 0, last_error TEXT NULL CHECK (length <= 512), created_at, updated_at)` with CHECKs tying `running` to a non-null lease and `done/dead` to a null lease, and an index on `(state, run_after)`; `JobId(i64)`.
  - `JobQueue::new(store: SqliteStore) -> JobQueue`
  - `enqueue(&self, kind: &str, domain: Option<DomainId>, payload: &serde_json::Value, run_after: Option<i64>) -> Result<JobId>`
  - `claim(&self, worker: &str, kinds: &[&str], lease_secs: u32) -> Result<Option<ClaimedJob>>` and `claim_at(.., now)`; atomically picks the oldest eligible `queued` job (or `running` with an expired lease), increments `epoch` and `attempts`, sets the lease. `ClaimedJob { id: JobId, kind: String, domain: Option<DomainId>, payload: serde_json::Value, epoch: i64, attempts: u32 }`.
  - `heartbeat(&self, id: JobId, epoch: i64, lease_secs: u32) -> Result<()>`, `complete(&self, id, epoch) -> Result<()>`, `fail(&self, id, epoch, error: &str, retry_after_secs: u32) -> Result<()>` (-> `queued` with new `run_after`, or `dead` once `attempts >= max_attempts`); each returns `Error::Conflict("stale job epoch")` when the epoch no longer matches.
  - `fence(tx: &mut SqliteTx, id: JobId, epoch: i64) -> Result<()>`: verifies inside the caller's write transaction that the job is `running` with this epoch and an unexpired lease, so a side effect and its fence check commit atomically.
  - `purge_finished(&self, older_than: i64) -> Result<u64>` (done/dead rows).
  - Limits as in Global Constraints; unknown/oversized kind or payload -> `Error::Invalid`.

- [ ] **Step 1: Write failing tests:** `enqueue_validates_kind_and_payload`, `claim_picks_oldest_eligible_and_respects_run_after`, `claim_filters_by_kind`, `two_workers_never_claim_the_same_job` (file DB, 4 connections, 200 jobs, 8 workers: every job claimed exactly once), `expired_lease_job_is_reclaimed_with_higher_epoch`, `zombie_worker_cannot_heartbeat_complete_or_fail` (Conflict after re-claim), `fence_rejects_stale_epoch_inside_transaction` (and passes for the current owner; side effect rolled back when fence fails), `fail_requeues_with_backoff_then_dead_at_max_attempts`, `last_error_is_truncated_and_sanitized` (control characters removed, <= 512), `heartbeat_extends_lease`, `purge_finished_only_removes_old_done_dead`, `domain_fk_enforced`, schema negative tests (running without lease rejected, bad state, oversized payload).
- [ ] **Step 2: Run to confirm FAIL.**
- [ ] **Step 3: Implement** migration + `jobs.rs` (all times from `unixepoch()`; `_at` twins for tests; `claim` is one write transaction so it is atomic on SQLite and maps to `FOR UPDATE SKIP LOCKED` on Postgres — note this in the module doc).
- [ ] **Step 4: Run focused tests several times (flake check), workspace tests, regenerate the snapshot; commit** `feat(store): job queue with leases and fencing`.

### Task P4: Hashed, revocable capabilities (rusty-directory)

**Files:**
- Create: `crates/rusty-store/migrations/<next>_capabilities.sql` (migrations live in rusty-store), `crates/rusty-directory/src/caps.rs`
- Modify: `crates/rusty-directory/src/lib.rs`, `crates/rusty-directory/src/dir.rs` (revocation hooks), `crates/rusty-store/src/gc.rs` (purge)
- Test: `crates/rusty-directory/tests/capabilities.rs`, schema snapshot

**Interfaces:**
- Consumes: `LocalAuthenticator::verify`, `Directory::set_password`, `Directory::set_status`.
- Produces:
  - Table `capabilities(token_hash BLOB(32) PRIMARY KEY, account_id, domain_id, role TEXT CHECK IN ('imap','jmap','dav','smtp-submit'), issued_at, expires_at, revoked_at NULL, FOREIGN KEY (account_id, domain_id) REFERENCES accounts(id, domain_id) ON DELETE CASCADE)` — if `accounts` lacks `UNIQUE(id, domain_id)` at this point, add it in this migration; `STRICT`; `CHECK (expires_at > issued_at AND expires_at - issued_at <= 86400)`.
  - `CapabilityRole` enum (`Imap, Jmap, Dav, SmtpSubmit`), `Capability(String)` newtype with 64 lowercase-hex chars (256-bit random from OsRng) that implements `Debug` WITHOUT printing the token.
  - `Directory::issue_capability(&self, account: AccountId, role: CapabilityRole, ttl_secs: u32) -> Result<Capability>` (ttl clamped to 1..=86400; at most 64 active per account, oldest revoked beyond that), `validate_capability(&self, token: &str, role: CapabilityRole) -> Result<Option<CapabilityInfo>>` where `CapabilityInfo { account: AccountId, domain: DomainId, role: CapabilityRole, expires_at: i64 }`, `revoke_capability(&self, token: &str)`, `revoke_all_capabilities(&self, account: AccountId) -> Result<u64>`; `_at(now)` twins.
  - Validation returns `Ok(None)` for malformed, unknown, expired, revoked, wrong-role tokens AND for accounts that are not `active` (locked/disabled), all through the same code path shape (constant-time hash comparison is done by the database lookup on the hash; do a dummy lookup for malformed tokens to keep the shape).
  - `set_password` and `set_status` (to non-active) call `revoke_all_capabilities` in the same transaction; account deletion cascades.
  - `purge_capabilities(store, older_than) -> Result<u64>` in gc.rs (expired or revoked longer than the retention), batches of 1000.

- [ ] **Step 1: Write failing tests:** `issue_validate_roundtrip`, `only_hash_is_stored` (the token string does not appear anywhere in the DB: scan all text/blob columns), `wrong_role_is_none`, `expired_is_none_via_validate_at`, `revoked_is_none`, `malformed_tokens_are_none_and_do_not_error` (empty, 63/65 chars, uppercase, non-hex, 1 MB string), `locked_account_capabilities_are_none`, `password_change_revokes_all`, `status_change_revokes_all`, `account_delete_cascades`, `per_account_cap_of_64_revokes_oldest`, `ttl_is_clamped`, `debug_does_not_print_token`, `purge_capabilities_removes_only_old_dead_rows`, `tokens_are_unique_over_10k_issues`, cross-tenant: a capability for account A never validates as another account, schema negative tests.
- [ ] **Step 2: Run to confirm FAIL.**
- [ ] **Step 3: Implement** migration, `caps.rs`, the hooks and the purge.
- [ ] **Step 4: Run focused tests, workspace tests, regenerate the snapshot; commit** `feat(directory): hashed revocable capabilities`.

### Task P5: Verification and docs

**Files:**
- Modify: `README.md`, `SECURITY.md` (<100 lines), spec statements only if code contradicts them (report, do not edit specs)

- [ ] **Step 1: Add** to SECURITY.md the new invariants (idempotent delivery, fencing, capability hashing/revocation, limits) and to the README crate map the new modules.
- [ ] **Step 2: Run** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo deny check`, `bash scripts/ci.sh`; report counts.
- [ ] **Step 3: Commit** `docs: distributed prep invariants`.
