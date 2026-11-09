//! Job queue with leases and fencing (distributed-model spec section 5).
//!
//! # Model
//!
//! A worker [`claim`](JobQueue::claim)s the oldest eligible job: `queued` with
//! `run_after <= now`, or `running` whose lease has lapsed (`lease_expires_at <= now`). A claim
//! increments both `attempts` and `epoch`; the epoch is the **fencing token**. Only the holder
//! of the current epoch, with an unexpired lease, may [`heartbeat`](JobQueue::heartbeat),
//! [`complete`](JobQueue::complete) or [`fail`](JobQueue::fail) the job; every other caller
//! (a zombie whose lease lapsed and whose job was re-claimed, an unknown id, a finished job)
//! gets the same `Conflict("stale job epoch")`, so the error is no oracle for which case it
//! was. The lease is the authority: an owner whose lease lapsed is refused even before anyone
//! re-claims, and the job is simply claimed again.
//!
//! A job out of attempts (`attempts >= max_attempts`, at most 20) becomes `dead`, either when
//! `fail` is called on its last attempt or when `claim` meets it with no attempt left (a lapsed
//! lease on the last attempt, or a queued row that somehow has none). `claim` settles such
//! rows one at a time as it walks the candidates, at most [`MAX_SETTLED_PER_CLAIM`] per call
//! (then it returns `None` and the next call continues), so one call never holds the write lock
//! for long and never rewrites the whole table. `done`, `failed` and `dead` rows are removed by
//! [`purge_finished`](JobQueue::purge_finished).
//!
//! # Contract for job steps (spec section 5)
//!
//! Delivery is **at least once**. **Every job step must be idempotent and resumable, including
//! its database effects**: a worker may crash between committing a fenced effect and calling
//! `complete`; the lease then lapses and the job runs again under a new epoch, which passes the
//! fence. A slow worker whose lease lapsed loses its work (its fence and its `complete` fail).
//! Heartbeat at most every `lease / 3`.
//!
//! The preferred pattern for a step whose effect is in this database is to pass the claimed
//! [`JobFence`] to the repository operation that performs it (`Mailbox::deliver_fenced`,
//! `deliver_many_fenced`): the operation fences (epoch, `running`, lease unexpired by the
//! database clock), does the effect and marks the job `done` in ONE transaction, so the effect
//! and the completion commit together or not at all. A separate `complete` after a committed
//! effect leaves the crash window described above.
//! Effects outside the database (files, mail, network) must be idempotent on their own.
//!
//! # Time
//!
//! Every decision uses the database clock (`unixepoch()` / `rs_now()`), read in the same statement as
//! the decision. Each time-dependent method has a `#[doc(hidden)]` `_at(.., now)` twin taking
//! unix seconds so tests never sleep; it trusts the caller's clock, production code calls the
//! plain method.
//!
//! # Limits and hostile input
//!
//! `kind` is `[a-z0-9._-]`, 1..=64 bytes. The payload is at most 64 KiB serialized and nested
//! at most [`MAX_PAYLOAD_DEPTH`] containers deep (checked iteratively before serializing, so
//! a hostile deeply nested value cannot overflow the stack; 64 is well inside the JSON parser's
//! default recursion limit, which stays in force, so every accepted payload reads back in
//! `claim`. A stored row that is unparseable or nested deeper than 64 (corrupt or legacy; found
//! by an iterative bracket scan before any recursive parse) is marked `dead` by `claim`, which
//! carries on with the next job). The `last_error` a
//! worker supplies is data: control, format and line-separator characters are removed and it
//! is cut to 512 bytes. Error messages are fixed strings and never contain caller input.
//!
//! Unbounded growth is capped: at most [`MAX_PENDING_JOBS_PER_DOMAIN`] `queued` or `running`
//! jobs per `domain_id`, and [`MAX_PENDING_JOBS_GLOBAL_NO_DOMAIN`] with `domain_id` NULL;
//! `enqueue` counts inside its write transaction and returns `Quota("job queue full")` at the
//! cap. Finished rows older than [`JOB_RETENTION_SECS`] are removed by
//! [`purge_finished_default`](JobQueue::purge_finished_default).
//!
//! # Backends
//!
//! The queue is a facade over [`JobRepo`] (SQLite and Postgres implementations, same semantics;
//! see `repo/jobs.rs` for the locking argument). On Postgres a claim takes the candidate with
//! `FOR UPDATE SKIP LOCKED` and every lease decision uses `statement_timestamp()` of the
//! deciding statement. A bulk "reap everything" statement without `SKIP LOCKED` would block
//! concurrent claimers; this queue deliberately has none (row-at-a-time settling).

use rusty_core::text::is_format_or_line_separator;
use rusty_core::{DomainId, Error, JobId, Result};
use serde_json::Value;

use std::sync::Arc;

use crate::gc::PURGE_BATCH;
use crate::repo::jobs::{ClaimRequest, NewJob};
use crate::repo::{JobFence, JobRepo, sqlite::SqliteRepo};
use crate::mailbox::check_supplied_time;
use crate::SqliteStore;

/// Largest serialized job payload, in bytes.
pub const MAX_JOB_PAYLOAD_BYTES: usize = 65536;
/// Deepest container nesting of a job payload (well below the JSON parser recursion limit).
pub const MAX_PAYLOAD_DEPTH: usize = 64;
/// Longest job kind, in bytes.
pub const MAX_JOB_KIND_BYTES: usize = 64;
/// Largest `max_attempts`.
pub const MAX_JOB_ATTEMPTS: u32 = 20;
/// `max_attempts` of a job enqueued without an explicit value.
pub const DEFAULT_JOB_ATTEMPTS: u32 = 5;
/// Longest lease (and longest `heartbeat` extension), in seconds.
pub const MAX_LEASE_SECS: u32 = 86400;
/// Longest `fail` back-off, in seconds (larger values are clamped).
pub const MAX_RETRY_AFTER_SECS: u32 = 7 * 86400;
/// Most kinds one `claim` may ask for.
pub const MAX_CLAIM_KINDS: usize = 16;
/// `last_error` is cut to this many bytes.
pub const MAX_LAST_ERROR_BYTES: usize = 512;
/// Most `queued` plus `running` jobs one domain may have.
pub const MAX_PENDING_JOBS_PER_DOMAIN: u32 = 10_000;
/// Most `queued` plus `running` jobs without a domain.
pub const MAX_PENDING_JOBS_GLOBAL_NO_DOMAIN: u32 = 10_000;
/// How long finished jobs are kept by default.
pub const JOB_RETENTION_SECS: i64 = 7 * 86400;
/// Most rows one `claim` call settles as `dead` before it returns `None`.
pub const MAX_SETTLED_PER_CLAIM: u32 = 100;
/// Longest worker name, in bytes.
const MAX_WORKER_BYTES: usize = 128;

const STALE: &str = "stale job epoch";

/// A job leased to a worker. `epoch` is the fencing token to present on every later call.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimedJob {
    pub id: JobId,
    pub kind: String,
    pub domain: Option<DomainId>,
    pub payload: Value,
    pub epoch: i64,
    /// Claims so far, including this one.
    pub attempts: u32,
    /// Database-clock time (unix seconds) the job was first enqueued; stable across retries.
    pub created_at: i64,
}

/// The job queue over a [`JobRepo`] (either backend).
#[derive(Clone)]
pub struct JobQueue {
    repo: Arc<dyn JobRepo>,
    max_pending_per_domain: u32,
    max_pending_no_domain: u32,
}

fn invalid(what: &'static str) -> Error {
    Error::Invalid(what.into())
}

pub(crate) fn stale() -> Error {
    Error::Conflict(STALE.into())
}

fn valid_kind(kind: &str) -> bool {
    !kind.is_empty()
        && kind.len() <= MAX_JOB_KIND_BYTES
        && kind
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

fn valid_worker(w: &str) -> bool {
    !w.is_empty()
        && w.len() <= MAX_WORKER_BYTES
        && w.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn valid_lease(secs: u32) -> bool {
    (1..=MAX_LEASE_SECS).contains(&secs)
}

/// Container nesting depth of `v` is at most [`MAX_PAYLOAD_DEPTH`]. Iterative: constant
/// call-stack use whatever the shape of `v`.
fn depth_ok(v: &Value) -> bool {
    let mut stack: Vec<(&Value, usize)> = vec![(v, 0)];
    while let Some((v, depth)) = stack.pop() {
        match v {
            Value::Array(items) => {
                if depth + 1 > MAX_PAYLOAD_DEPTH {
                    return false;
                }
                stack.extend(items.iter().map(|c| (c, depth + 1)));
            }
            Value::Object(map) => {
                if depth + 1 > MAX_PAYLOAD_DEPTH {
                    return false;
                }
                stack.extend(map.values().map(|c| (c, depth + 1)));
            }
            _ => {}
        }
    }
    true
}

/// Nesting depth of the JSON text `t` (brackets outside strings) is at most
/// [`MAX_PAYLOAD_DEPTH`]. Iterative; `t` is already known to be valid JSON.
fn text_depth_ok(t: &str) -> bool {
    let (mut depth, mut in_str, mut esc) = (0usize, false, false);
    for b in t.bytes() {
        if in_str {
            match (esc, b) {
                (true, _) => esc = false,
                (false, b'\\') => esc = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > MAX_PAYLOAD_DEPTH {
                    return false;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    true
}

/// Parse stored payload text with the default parser limits. The iterative [`text_depth_ok`]
/// scan runs first, so a corrupt or legacy row nested deeper than [`MAX_PAYLOAD_DEPTH`] is
/// refused without any recursion.
pub(crate) fn parse_payload(t: &str) -> Option<Value> {
    if !text_depth_ok(t) {
        return None;
    }
    serde_json::from_str(t).ok()
}

/// Remove control, format, line-separator and tag characters, then cut to
/// [`MAX_LAST_ERROR_BYTES`] on a character boundary.
fn sanitize_error(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_control() || is_format_or_line_separator(c) {
            continue;
        }
        if out.len() + c.len_utf8() > MAX_LAST_ERROR_BYTES {
            break;
        }
        out.push(c);
    }
    out
}


/// Depth first, iteratively (serializing a hostile deeply nested value could overflow the stack),
/// then the size cap.
fn serialize_payload(payload: &Value) -> Result<String> {
    if !depth_ok(payload) {
        return Err(invalid("job payload nested too deeply"));
    }
    let text = serde_json::to_string(payload).map_err(|_| invalid("invalid job payload"))?;
    if text.len() > MAX_JOB_PAYLOAD_BYTES {
        return Err(invalid("job payload too large"));
    }
    Ok(text)
}

impl JobQueue {
    /// A queue over a SQLite store.
    pub fn new(store: SqliteStore) -> JobQueue {
        Self::with_repo(Arc::new(SqliteRepo::new(store)))
    }

    /// A queue over any backend's [`JobRepo`].
    pub fn with_repo(repo: Arc<dyn JobRepo>) -> JobQueue {
        JobQueue {
            repo,
            max_pending_per_domain: MAX_PENDING_JOBS_PER_DOMAIN,
            max_pending_no_domain: MAX_PENDING_JOBS_GLOBAL_NO_DOMAIN,
        }
    }

    /// A queue with smaller pending-job caps (for tests; production uses [`new`](Self::new)).
    #[doc(hidden)]
    pub fn with_limits(
        store: SqliteStore,
        max_pending_per_domain: u32,
        max_pending_no_domain: u32,
    ) -> JobQueue {
        Self::with_repo_and_limits(
            Arc::new(SqliteRepo::new(store)),
            max_pending_per_domain,
            max_pending_no_domain,
        )
    }

    /// [`with_limits`](Self::with_limits) over any backend's repository.
    #[doc(hidden)]
    pub fn with_repo_and_limits(
        repo: Arc<dyn JobRepo>,
        max_pending_per_domain: u32,
        max_pending_no_domain: u32,
    ) -> JobQueue {
        JobQueue {
            repo,
            max_pending_per_domain,
            max_pending_no_domain,
        }
    }

    /// Queue a job with the default attempt cap ([`DEFAULT_JOB_ATTEMPTS`]). `run_after`
    /// (unix seconds) defaults to now. Unknown/oversized `kind` or `payload` is `Invalid`; a
    /// full queue (see the module docs) is `Quota("job queue full")`.
    pub async fn enqueue(
        &self,
        kind: &str,
        domain: Option<DomainId>,
        payload: &Value,
        run_after: Option<i64>,
    ) -> Result<JobId> {
        self.enqueue_inner(kind, domain, payload, run_after, DEFAULT_JOB_ATTEMPTS, None)
            .await
    }

    /// [`enqueue`](Self::enqueue) at time `now`.
    #[doc(hidden)]
    pub async fn enqueue_at(
        &self,
        kind: &str,
        domain: Option<DomainId>,
        payload: &Value,
        run_after: Option<i64>,
        now: i64,
    ) -> Result<JobId> {
        check_supplied_time(now)?;
        self.enqueue_inner(kind, domain, payload, run_after, DEFAULT_JOB_ATTEMPTS, Some(now))
            .await
    }

    /// [`enqueue`](Self::enqueue) with an explicit `max_attempts` in `1..=`[`MAX_JOB_ATTEMPTS`].
    pub async fn enqueue_with_max_attempts(
        &self,
        kind: &str,
        domain: Option<DomainId>,
        payload: &Value,
        run_after: Option<i64>,
        max_attempts: u32,
    ) -> Result<JobId> {
        self.enqueue_inner(kind, domain, payload, run_after, max_attempts, None)
            .await
    }

    /// [`enqueue_with_max_attempts`](Self::enqueue_with_max_attempts) at time `now`.
    #[doc(hidden)]
    pub async fn enqueue_with_max_attempts_at(
        &self,
        kind: &str,
        domain: Option<DomainId>,
        payload: &Value,
        run_after: Option<i64>,
        max_attempts: u32,
        now: i64,
    ) -> Result<JobId> {
        check_supplied_time(now)?;
        self.enqueue_inner(kind, domain, payload, run_after, max_attempts, Some(now))
            .await
    }

    async fn enqueue_inner(
        &self,
        kind: &str,
        domain: Option<DomainId>,
        payload: &Value,
        run_after: Option<i64>,
        max_attempts: u32,
        now: Option<i64>,
    ) -> Result<JobId> {
        if !valid_kind(kind) {
            return Err(invalid("invalid job kind"));
        }
        if !(1..=MAX_JOB_ATTEMPTS).contains(&max_attempts) {
            return Err(invalid("invalid job attempt limit"));
        }
        let text = serialize_payload(payload)?;
        let cap = match domain {
            Some(_) => self.max_pending_per_domain,
            None => self.max_pending_no_domain,
        };
        self.repo
            .enqueue(&NewJob {
                kind,
                domain,
                payload: &text,
                run_after,
                max_attempts,
                cap,
                now,
            })
            .await
    }

    /// Lease the oldest eligible job of one of `kinds` to `worker` for `lease_secs`
    /// (`1..=`[`MAX_LEASE_SECS`]), or `None` if there is none (or only exhausted rows were
    /// found; see the module docs). The whole claim is one write transaction.
    pub async fn claim(
        &self,
        worker: &str,
        kinds: &[&str],
        lease_secs: u32,
    ) -> Result<Option<ClaimedJob>> {
        self.claim_inner(worker, kinds, lease_secs, None).await
    }

    /// [`claim`](Self::claim) at time `now`.
    #[doc(hidden)]
    pub async fn claim_at(
        &self,
        worker: &str,
        kinds: &[&str],
        lease_secs: u32,
        now: i64,
    ) -> Result<Option<ClaimedJob>> {
        check_supplied_time(now)?;
        self.claim_inner(worker, kinds, lease_secs, Some(now)).await
    }

    async fn claim_inner(
        &self,
        worker: &str,
        kinds: &[&str],
        lease_secs: u32,
        now: Option<i64>,
    ) -> Result<Option<ClaimedJob>> {
        if !valid_worker(worker) {
            return Err(invalid("invalid worker name"));
        }
        if !valid_lease(lease_secs) {
            return Err(invalid("invalid lease length"));
        }
        if kinds.is_empty() || kinds.len() > MAX_CLAIM_KINDS || !kinds.iter().all(|k| valid_kind(k))
        {
            return Err(invalid("invalid job kinds"));
        }
        self.repo
            .claim(&ClaimRequest {
                worker,
                kinds,
                lease_secs,
                now,
            })
            .await
    }

    /// Extend the lease to `now + lease_secs`. `Conflict("stale job epoch")` unless the job is
    /// `running` with this epoch and an unexpired lease. Call at least every `lease / 3`.
    pub async fn heartbeat(&self, id: JobId, epoch: i64, lease_secs: u32) -> Result<()> {
        self.heartbeat_inner(id, epoch, lease_secs, None).await
    }

    /// [`heartbeat`](Self::heartbeat) at time `now`.
    #[doc(hidden)]
    pub async fn heartbeat_at(
        &self,
        id: JobId,
        epoch: i64,
        lease_secs: u32,
        now: i64,
    ) -> Result<()> {
        check_supplied_time(now)?;
        self.heartbeat_inner(id, epoch, lease_secs, Some(now)).await
    }

    async fn heartbeat_inner(
        &self,
        id: JobId,
        epoch: i64,
        lease_secs: u32,
        now: Option<i64>,
    ) -> Result<()> {
        if !valid_lease(lease_secs) {
            return Err(invalid("invalid lease length"));
        }
        self.repo.heartbeat(id, epoch, lease_secs, now).await
    }

    /// Mark the job `done`. `Conflict("stale job epoch")` as for [`heartbeat`](Self::heartbeat).
    /// For a step with a database effect pass a [`JobFence`] to the repository operation that
    /// performs it (module docs) instead.
    pub async fn complete(&self, id: JobId, epoch: i64) -> Result<()> {
        self.repo.complete(id, epoch, None).await
    }

    /// [`complete`](Self::complete) at time `now`.
    #[doc(hidden)]
    pub async fn complete_at(&self, id: JobId, epoch: i64, now: i64) -> Result<()> {
        check_supplied_time(now)?;
        self.repo.complete(id, epoch, Some(now)).await
    }

    /// Record a failed attempt: the job goes back to `queued` with
    /// `run_after = now + retry_after_secs` (clamped to [`MAX_RETRY_AFTER_SECS`]), or to `dead`
    /// once `attempts >= max_attempts`. `error` is sanitized and truncated (module docs) and
    /// never appears in an error message. `Conflict("stale job epoch")` as for
    /// [`heartbeat`](Self::heartbeat).
    pub async fn fail(
        &self,
        id: JobId,
        epoch: i64,
        error: &str,
        retry_after_secs: u32,
    ) -> Result<()> {
        self.fail_inner(id, epoch, error, retry_after_secs, None)
            .await
    }

    /// [`fail`](Self::fail) at time `now`.
    #[doc(hidden)]
    pub async fn fail_at(
        &self,
        id: JobId,
        epoch: i64,
        error: &str,
        retry_after_secs: u32,
        now: i64,
    ) -> Result<()> {
        check_supplied_time(now)?;
        self.fail_inner(id, epoch, error, retry_after_secs, Some(now))
            .await
    }

    async fn fail_inner(
        &self,
        id: JobId,
        epoch: i64,
        error: &str,
        retry_after_secs: u32,
        now: Option<i64>,
    ) -> Result<()> {
        let error = sanitize_error(error);
        let retry = i64::from(retry_after_secs.min(MAX_RETRY_AFTER_SECS));
        self.repo.fail(id, epoch, &error, retry, now).await
    }

    /// Requeue the claimed job after `retry_after_secs` (clamped like [`fail`](Self::fail)) with a
    /// replaced payload and the attempt counter reset, so the retry schedule is not bounded by
    /// `max_attempts` (which then only limits claims that end without a decision, such as crashes).
    /// Same fencing as [`fail`](Self::fail); the payload obeys the enqueue limits.
    pub async fn reschedule(
        &self,
        id: JobId,
        epoch: i64,
        payload: &Value,
        error: &str,
        retry_after_secs: u32,
    ) -> Result<()> {
        self.reschedule_inner(id, epoch, payload, error, retry_after_secs, None).await
    }

    /// [`reschedule`](Self::reschedule) at time `now`.
    #[doc(hidden)]
    pub async fn reschedule_at(
        &self,
        id: JobId,
        epoch: i64,
        payload: &Value,
        error: &str,
        retry_after_secs: u32,
        now: i64,
    ) -> Result<()> {
        check_supplied_time(now)?;
        self.reschedule_inner(id, epoch, payload, error, retry_after_secs, Some(now)).await
    }

    async fn reschedule_inner(
        &self,
        id: JobId,
        epoch: i64,
        payload: &Value,
        error: &str,
        retry_after_secs: u32,
        now: Option<i64>,
    ) -> Result<()> {
        let text = serialize_payload(payload)?;
        let error = sanitize_error(error);
        let retry = i64::from(retry_after_secs.min(MAX_RETRY_AFTER_SECS));
        self.repo.reschedule(id, epoch, &text, &error, retry, now).await
    }

    /// The database clock in unix seconds: the time every lease and schedule decision uses.
    pub async fn now(&self) -> Result<i64> {
        self.repo.db_now().await
    }

    /// Check, in its own transaction, that the job is `running` with this `epoch` and an
    /// unexpired lease; otherwise `Conflict("stale job epoch")`. A check is not a fence: it
    /// holds nothing after it returns. A step with a database effect passes the [`JobFence`] to
    /// the repository operation instead, so fence, effect and completion commit together.
    pub async fn fence_check(&self, fence: JobFence) -> Result<()> {
        self.repo.fence_check(fence, None).await
    }

    /// [`fence_check`](Self::fence_check) at time `now`.
    #[doc(hidden)]
    pub async fn fence_check_at(&self, fence: JobFence, now: i64) -> Result<()> {
        check_supplied_time(now)?;
        self.repo.fence_check(fence, Some(now)).await
    }

    /// Delete `done`, `failed` and `dead` jobs with `updated_at < older_than` (unix seconds),
    /// at most [`PURGE_BATCH`] per transaction, until none are left. Returns the rows removed.
    pub async fn purge_finished(&self, older_than: i64) -> Result<u64> {
        let mut total = 0u64;
        loop {
            let n = self.repo.purge_finished_batch(older_than, PURGE_BATCH).await?;
            total += n;
            if n < PURGE_BATCH as u64 {
                return Ok(total);
            }
        }
    }

    /// [`purge_finished`](Self::purge_finished) for rows older than `retention_secs` before
    /// `now` (unix seconds).
    #[doc(hidden)]
    pub async fn purge_finished_at(&self, now: i64, retention_secs: i64) -> Result<u64> {
        self.purge_finished(now.saturating_sub(retention_secs.max(0)))
            .await
    }

    /// [`purge_finished`](Self::purge_finished) with the database clock and
    /// [`JOB_RETENTION_SECS`].
    pub async fn purge_finished_default(&self) -> Result<u64> {
        let now = self.repo.db_now().await?;
        self.purge_finished_at(now, JOB_RETENTION_SECS).await
    }
}
