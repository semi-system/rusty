//! `JobRepo`: the backend-neutral job repository (PG5).
//!
//! # Boundary
//!
//! As for the other repositories, one method is ONE unit of work that owns its transaction (the
//! Postgres implementation re-runs it on 40P01 / 40001, `tx_retry!`). The [`JobQueue`]
//! (crate::JobQueue) facade keeps validation (kind, worker, lease, payload depth and size), the
//! sanitising of `last_error`, the supplied-time bound of the `_at` twins and the purge loops.
//!
//! # Semantics (identical on both backends)
//!
//! A claim increments `attempts` and `epoch` (the fencing token). Only the holder of the current
//! epoch with an unexpired lease may heartbeat, complete or fail; everything else is
//! `Conflict("stale job epoch")` (no oracle which case it was). The lease is decided by the
//! DATABASE clock (`{NOW}`: `unixepoch()` / `rs_now()`), in the same statement that acts on it;
//! `now: Some(t)` (test twins) replaces the clock. A job out of attempts becomes `dead`.
//!
//! # Locking
//!
//! Job operations are not mailbox scoped: there is no mailbox lock. The lock order of the whole
//! store is mailbox row, then blob rows (ascending hash), then the job row; every method here
//! touches only job rows (and, for `enqueue`, one cap-serialisation lock taken first and
//! nothing else), so it cannot take part in a cycle with the mailbox paths, whose last lock is
//! the job row of their fence.
//!
//! * `claim` (Postgres): `SELECT .. ORDER BY run_after, id LIMIT 1 FOR UPDATE OF j SKIP LOCKED`
//!   per candidate, so concurrent claimers skip rows another transaction is claiming instead of
//!   blocking; the loop settles exhausted or unreadable rows as `dead` exactly as the SQLite
//!   walk does (at most `MAX_SETTLED_PER_CLAIM` per call).
//! * `enqueue` counts pending jobs and inserts in one transaction. SQLite's single writer makes
//!   that atomic; on Postgres two concurrent enqueues could both slip under the cap, so the
//!   transaction first serialises per scope: `SELECT .. FROM domains WHERE id = $1 FOR NO KEY
//!   UPDATE` for a domain job (does not block foreign-key checks of other tables), and the
//!   transaction-scoped `pg_advisory_xact_lock` for jobs without a domain. Transaction-scoped,
//!   so it is released at COMMIT/ROLLBACK and is safe behind a transaction-pooling proxy
//!   (decision D6); no session advisory lock is ever used.
//!
//! # Fence inventory (the `complete_in` call sites)
//!
//! `JobQueue::complete_in` / `fence` took a transaction handle. They are gone; a step with a
//! database effect passes `Option<JobFence>` to the mailbox operation that performs it
//! (`append` / `deliver` and `deliver_many`, see `MailboxRepo`), and the fence + `done` happen
//! in that operation's transaction (`COMPLETE_JOB`: epoch, `running`, lease unexpired by the
//! database clock). [`JobRepo::fence_check`] is the standalone check, in its own transaction,
//! for callers that only need to know whether they still own the job. No other repository
//! operation is used by a worker kind today (there is no worker crate yet).

use async_trait::async_trait;
use rusty_core::{DomainId, JobId, Result};

use super::JobFence;
use crate::jobs::ClaimedJob;

/// A validated job to insert. `payload` is the serialized JSON (already depth and size checked).
#[derive(Debug, Clone, Copy)]
pub struct NewJob<'a> {
    pub kind: &'a str,
    pub domain: Option<DomainId>,
    pub payload: &'a str,
    /// `None` = now (database clock).
    pub run_after: Option<i64>,
    pub max_attempts: u32,
    /// Most `queued` + `running` jobs of this scope (per domain, or without a domain).
    pub cap: u32,
    /// `None` = database clock; `Some` only from the test twins.
    pub now: Option<i64>,
}

/// A validated claim request.
#[derive(Debug, Clone, Copy)]
pub struct ClaimRequest<'a> {
    pub worker: &'a str,
    pub kinds: &'a [&'a str],
    pub lease_secs: u32,
    pub now: Option<i64>,
}

#[async_trait]
pub trait JobRepo: Send + Sync {
    /// The database clock in unix seconds (a read).
    async fn db_now(&self) -> Result<i64>;
    /// Insert a job; `Quota("job queue full")` at the cap.
    async fn enqueue(&self, job: &NewJob<'_>) -> Result<JobId>;
    /// Lease the oldest eligible job of one of the kinds, or `None`.
    async fn claim(&self, req: &ClaimRequest<'_>) -> Result<Option<ClaimedJob>>;
    /// Extend the lease to `now + lease_secs`; stale unless the caller owns an unexpired lease.
    async fn heartbeat(&self, id: JobId, epoch: i64, lease_secs: u32, now: Option<i64>) -> Result<()>;
    /// Mark `done`; stale as above.
    async fn complete(&self, id: JobId, epoch: i64, now: Option<i64>) -> Result<()>;
    /// Record a failed attempt (`error` already sanitised): requeue after `retry_secs` or `dead`.
    async fn fail(&self, id: JobId, epoch: i64, error: &str, retry_secs: i64, now: Option<i64>) -> Result<()>;
    /// The caller still owns the job (epoch, `running`, unexpired lease); a read.
    async fn fence_check(&self, fence: JobFence, now: Option<i64>) -> Result<()>;
    /// Delete up to `limit` finished (`done`, `failed`, `dead`) jobs with `updated_at <
    /// older_than`; returns how many.
    async fn purge_finished_batch(&self, older_than: i64, limit: i64) -> Result<u64>;
}

/// Statement text shared by both backends (`{NOW}` = database clock, `$N` placeholders).
pub(crate) mod sql {
    pub const HEARTBEAT: &str = "UPDATE jobs SET lease_expires_at = COALESCE($4, {NOW}) + $3, \
        updated_at = COALESCE($4, {NOW}) \
        WHERE id = $1 AND epoch = $2 AND state = 'running' AND lease_expires_at > COALESCE($4, {NOW})";
    pub const COMPLETE: &str = "UPDATE jobs SET state = 'done', lease_owner = NULL, \
        lease_expires_at = NULL, updated_at = COALESCE($3, {NOW}) \
        WHERE id = $1 AND epoch = $2 AND state = 'running' AND lease_expires_at > COALESCE($3, {NOW})";
    pub const FAIL: &str = "UPDATE jobs SET \
        state = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'queued' END, \
        run_after = CASE WHEN attempts >= max_attempts THEN run_after ELSE COALESCE($5, {NOW}) + $4 END, \
        lease_owner = NULL, lease_expires_at = NULL, last_error = $3, updated_at = COALESCE($5, {NOW}) \
        WHERE id = $1 AND epoch = $2 AND state = 'running' AND lease_expires_at > COALESCE($5, {NOW})";
    pub const FENCE: &str = "SELECT CAST(1 AS BIGINT) FROM jobs \
        WHERE id = $1 AND epoch = $2 AND state = 'running' AND lease_expires_at > COALESCE($3, {NOW})";
    pub const SETTLE_DEAD: &str = "UPDATE jobs SET state = 'dead', lease_owner = NULL, \
        lease_expires_at = NULL, last_error = $2, updated_at = $3 WHERE id = $1";
    pub const CLAIM_UPDATE: &str = "UPDATE jobs SET state = 'running', attempts = attempts + 1, \
        epoch = epoch + 1, lease_owner = $2, lease_expires_at = $3, updated_at = $4 \
        WHERE id = $1 RETURNING id, kind, domain_id, payload, epoch, attempts";
}

/// `Ok` iff exactly one row was affected by a fenced update.
pub(crate) fn require_one(rows: u64) -> Result<()> {
    if rows == 1 {
        Ok(())
    } else {
        Err(crate::jobs::stale())
    }
}

/// Why a candidate that cannot run again is parked.
pub(crate) fn exhausted_reason(state_is_running: bool) -> &'static str {
    if state_is_running {
        "lease expired on the last attempt"
    } else {
        "no attempts left"
    }
}
