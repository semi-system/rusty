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
//! The preferred pattern for a step whose effect is in this database is
//! [`JobQueue::complete_in`]: in one write transaction do the effect, then `complete_in`
//! (fence plus mark `done`), then commit. The effect and the completion then commit together or
//! not at all, so the database effect happens exactly once. [`JobQueue::fence`] alone (effect
//! committed, `complete` called separately) leaves the crash window described above.
//! Effects outside the database (files, mail, network) must be idempotent on their own.
//!
//! # Time
//!
//! Every decision uses the database clock (`unixepoch()`), read inside the same transaction as
//! the decision. Each time-dependent method has a `#[doc(hidden)]` `_at(.., now)` twin taking
//! unix seconds so tests never sleep; it trusts the caller's clock, production code calls the
//! plain method.
//!
//! # Limits and hostile input
//!
//! `kind` is `[a-z0-9._-]`, 1..=64 bytes. The payload is at most 64 KiB serialized and nested
//! at most [`MAX_JOB_PAYLOAD_DEPTH`] containers deep (checked iteratively before serializing, so
//! a hostile deeply nested value cannot overflow the stack; the depth equals the JSON parser's
//! recursion limit, so every accepted payload can be read back by `claim`; if a row nevertheless
//! holds an unparseable payload, `claim` marks it `dead` and carries on). The `last_error` a
//! worker supplies is data: control, format and line-separator characters are removed and it
//! is cut to 512 bytes. Error messages are fixed strings and never contain caller input.
//!
//! Unbounded growth is capped: at most [`MAX_PENDING_JOBS_PER_DOMAIN`] `queued` or `running`
//! jobs per `domain_id`, and [`MAX_PENDING_JOBS_GLOBAL_NO_DOMAIN`] with `domain_id` NULL;
//! `enqueue` counts inside its write transaction and returns `Quota("job queue full")` at the
//! cap. Finished rows older than [`JOB_RETENTION_SECS`] are removed by
//! [`purge_finished_default`](JobQueue::purge_finished_default).
//!
//! # Postgres port
//!
//! `claim` is one `BEGIN IMMEDIATE` transaction on SQLite: `SELECT` the first eligible row,
//! settle or `UPDATE` it. On Postgres the same shape maps 1:1 to one transaction in which the
//! `SELECT ... ORDER BY run_after, id LIMIT 1` gains `FOR UPDATE SKIP LOCKED` (concurrent
//! claimers skip rows another transaction is claiming instead of blocking). `unixepoch()` becomes
//! the database clock; note that `now()` is the *transaction start* time there, so use
//! `statement_timestamp()` for lease-expiry checks inside long caller transactions (`fence`,
//! `complete_in`). A bulk "reap everything" statement without `SKIP LOCKED` would block
//! concurrent claimers; this queue deliberately has none (row-at-a-time settling). `fence` and
//! `complete_in` must lock the job row (`SELECT ... FOR SHARE` / `FOR UPDATE`) on Postgres; on
//! SQLite the caller's `BEGIN IMMEDIATE` already serializes them against `claim`.

use rusty_core::text::is_format_or_line_separator;
use rusty_core::{DomainId, Error, JobId, Result};
use serde_json::Value;
use sqlx::{Row, SqliteConnection};

use crate::gc::PURGE_BATCH;
use crate::{map_db_error, SqliteStore, SqliteTx};

/// Largest serialized job payload, in bytes.
pub const MAX_JOB_PAYLOAD_BYTES: usize = 65536;
/// Deepest container nesting of a job payload (the JSON parser's recursion limit).
pub const MAX_JOB_PAYLOAD_DEPTH: usize = 128;
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
}

/// The job queue over a [`SqliteStore`].
#[derive(Debug, Clone)]
pub struct JobQueue {
    store: SqliteStore,
    max_pending_per_domain: u32,
    max_pending_no_domain: u32,
}

fn invalid(what: &'static str) -> Error {
    Error::Invalid(what.into())
}

fn stale() -> Error {
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

/// Container nesting depth of `v` is at most [`MAX_JOB_PAYLOAD_DEPTH`]. Iterative: constant
/// call-stack use whatever the shape of `v`.
fn depth_ok(v: &Value) -> bool {
    let mut stack: Vec<(&Value, usize)> = vec![(v, 0)];
    while let Some((v, depth)) = stack.pop() {
        match v {
            Value::Array(items) => {
                if depth + 1 > MAX_JOB_PAYLOAD_DEPTH {
                    return false;
                }
                stack.extend(items.iter().map(|c| (c, depth + 1)));
            }
            Value::Object(map) => {
                if depth + 1 > MAX_JOB_PAYLOAD_DEPTH {
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
/// [`MAX_JOB_PAYLOAD_DEPTH`]. Iterative; `t` is already known to be valid JSON.
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
                if depth > MAX_JOB_PAYLOAD_DEPTH {
                    return false;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    true
}

/// Parse stored payload text. The parser's own recursion limit (127 containers) is lifted
/// after [`text_depth_ok`] bounded the nesting at [`MAX_JOB_PAYLOAD_DEPTH`] (128), so every
/// payload `enqueue` accepts reads back and a deeper stored row is refused without recursion.
fn parse_payload(t: &str) -> Option<Value> {
    if !text_depth_ok(t) {
        return None;
    }
    let mut de = serde_json::Deserializer::from_str(t);
    de.disable_recursion_limit();
    let v = <Value as serde::Deserialize>::deserialize(&mut de).ok()?;
    de.end().ok()?;
    Some(v)
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

async fn db_now(conn: &mut SqliteConnection) -> Result<i64> {
    sqlx::query_scalar("SELECT unixepoch()")
        .fetch_one(conn)
        .await
        .map_err(map_db_error)
}

async fn clock(conn: &mut SqliteConnection, now: Option<i64>) -> Result<i64> {
    match now {
        Some(n) => Ok(n),
        None => db_now(conn).await,
    }
}

/// `Ok` iff exactly one row was affected by a fenced update.
fn require_one(rows: u64) -> Result<()> {
    if rows == 1 {
        Ok(())
    } else {
        Err(stale())
    }
}

/// Mark the job `done` if `epoch` still owns it with an unexpired lease.
async fn complete_on(conn: &mut SqliteConnection, id: JobId, epoch: i64, now: i64) -> Result<()> {
    let n = sqlx::query(
        "UPDATE jobs SET state = 'done', lease_owner = NULL, lease_expires_at = NULL, \
                updated_at = ? \
         WHERE id = ? AND epoch = ? AND state = 'running' AND lease_expires_at > ?",
    )
    .bind(now)
    .bind(id.0)
    .bind(epoch)
    .bind(now)
    .execute(conn)
    .await
    .map_err(map_db_error)?
    .rows_affected();
    require_one(n)
}

/// Park a row that can never run again.
async fn settle_dead(
    conn: &mut SqliteConnection,
    id: i64,
    reason: &'static str,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE jobs SET state = 'dead', lease_owner = NULL, lease_expires_at = NULL, \
                last_error = ?, updated_at = ? WHERE id = ?",
    )
    .bind(reason)
    .bind(now)
    .bind(id)
    .execute(conn)
    .await
    .map_err(map_db_error)?;
    Ok(())
}

impl JobQueue {
    pub fn new(store: SqliteStore) -> JobQueue {
        JobQueue {
            store,
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
        JobQueue {
            store,
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
        self.enqueue_inner(
            kind,
            domain,
            payload,
            run_after,
            DEFAULT_JOB_ATTEMPTS,
            Some(now),
        )
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
        // Depth first, iteratively: serializing a hostile deeply nested value could overflow
        // the stack.
        if !depth_ok(payload) {
            return Err(invalid("job payload nested too deeply"));
        }
        let text = serde_json::to_string(payload).map_err(|_| invalid("invalid job payload"))?;
        if text.len() > MAX_JOB_PAYLOAD_BYTES {
            return Err(invalid("job payload too large"));
        }
        let mut tx = self.store.begin_write().await?;
        let now = clock(&mut tx, now).await?;
        // The count and the insert share one write transaction: concurrent enqueues cannot
        // both slip under the cap.
        let (pending, cap): (i64, u32) = match domain {
            Some(d) => (
                sqlx::query_scalar(
                    "SELECT count(*) FROM jobs \
                     WHERE domain_id = ? AND state IN ('queued', 'running')",
                )
                .bind(d.0)
                .fetch_one(&mut *tx)
                .await
                .map_err(map_db_error)?,
                self.max_pending_per_domain,
            ),
            None => (
                sqlx::query_scalar(
                    "SELECT count(*) FROM jobs \
                     WHERE domain_id IS NULL AND state IN ('queued', 'running')",
                )
                .fetch_one(&mut *tx)
                .await
                .map_err(map_db_error)?,
                self.max_pending_no_domain,
            ),
        };
        if pending >= i64::from(cap) {
            return Err(Error::Quota("job queue full".into()));
        }
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO jobs (kind, domain_id, state, payload, attempts, max_attempts, \
                               run_after, epoch, created_at, updated_at) \
             VALUES (?, ?, 'queued', ?, 0, ?, ?, 0, ?, ?) RETURNING id",
        )
        .bind(kind)
        .bind(domain.map(|d| d.0))
        .bind(&text)
        .bind(i64::from(max_attempts))
        .bind(run_after.unwrap_or(now))
        .bind(now)
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(JobId(id))
    }

    /// Lease the oldest eligible job of one of `kinds` to `worker` for `lease_secs`
    /// (`1..=`[`MAX_LEASE_SECS`]), or `None` if there is none (or only exhausted rows were
    /// found; see the module docs). The whole claim is one write transaction (see the module
    /// docs for the Postgres mapping).
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
        // Only `?` placeholders are interpolated; every kind is bound.
        let placeholders = vec!["?"; kinds.len()].join(", ");
        let select = format!(
            "SELECT id, state, attempts, max_attempts FROM jobs WHERE kind IN ({placeholders}) \
             AND ((state = 'queued' AND run_after <= ?) \
               OR (state = 'running' AND lease_expires_at <= ?)) \
             ORDER BY run_after, id LIMIT 1"
        );

        let mut tx = self.store.begin_write().await?;
        let now = clock(&mut tx, now).await?;
        let mut settled = 0u32;
        let claimed = loop {
            if settled >= MAX_SETTLED_PER_CLAIM {
                break None;
            }
            let mut q = sqlx::query(&select);
            for k in kinds {
                q = q.bind(*k);
            }
            let Some(cand) = q
                .bind(now)
                .bind(now)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_db_error)?
            else {
                break None;
            };
            let id: i64 = cand.try_get("id").map_err(map_db_error)?;
            let state: String = cand.try_get("state").map_err(map_db_error)?;
            let attempts: i64 = cand.try_get("attempts").map_err(map_db_error)?;
            let max_attempts: i64 = cand.try_get("max_attempts").map_err(map_db_error)?;
            if attempts >= max_attempts {
                let why = if state == "running" {
                    "lease expired on the last attempt"
                } else {
                    "no attempts left"
                };
                settle_dead(&mut tx, id, why, now).await?;
                settled += 1;
                continue;
            }
            let row = sqlx::query(
                "UPDATE jobs SET state = 'running', attempts = attempts + 1, \
                        epoch = epoch + 1, lease_owner = ?, lease_expires_at = ?, \
                        updated_at = ? \
                 WHERE id = ? \
                 RETURNING id, kind, domain_id, payload, epoch, attempts",
            )
            .bind(worker)
            .bind(now.saturating_add(i64::from(lease_secs)))
            .bind(now)
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(map_db_error)?;
            let text: String = row.try_get("payload").map_err(map_db_error)?;
            let attempts: i64 = row.try_get("attempts").map_err(map_db_error)?;
            match parse_payload(&text) {
                Some(payload) => {
                    break Some(ClaimedJob {
                        id: JobId(row.try_get("id").map_err(map_db_error)?),
                        kind: row.try_get("kind").map_err(map_db_error)?,
                        domain: row
                            .try_get::<Option<i64>, _>("domain_id")
                            .map_err(map_db_error)?
                            .map(DomainId),
                        payload,
                        epoch: row.try_get("epoch").map_err(map_db_error)?,
                        attempts: u32::try_from(attempts)
                            .map_err(|_| Error::storage("job attempts out of range"))?,
                    });
                }
                None => {
                    // A row that can never be handed to a worker: park it and keep going.
                    settle_dead(&mut tx, id, "unreadable payload", now).await?;
                    settled += 1;
                }
            }
        };
        tx.commit().await.map_err(map_db_error)?;
        Ok(claimed)
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
        let mut tx = self.store.begin_write().await?;
        let now = clock(&mut tx, now).await?;
        let n = sqlx::query(
            "UPDATE jobs SET lease_expires_at = ?, updated_at = ? \
             WHERE id = ? AND epoch = ? AND state = 'running' AND lease_expires_at > ?",
        )
        .bind(now.saturating_add(i64::from(lease_secs)))
        .bind(now)
        .bind(id.0)
        .bind(epoch)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?
        .rows_affected();
        require_one(n)?;
        tx.commit().await.map_err(map_db_error)
    }

    /// Mark the job `done`. `Conflict("stale job epoch")` as for [`heartbeat`](Self::heartbeat).
    /// For a step with a database effect prefer [`complete_in`](Self::complete_in).
    pub async fn complete(&self, id: JobId, epoch: i64) -> Result<()> {
        self.complete_inner(id, epoch, None).await
    }

    /// [`complete`](Self::complete) at time `now`.
    #[doc(hidden)]
    pub async fn complete_at(&self, id: JobId, epoch: i64, now: i64) -> Result<()> {
        self.complete_inner(id, epoch, Some(now)).await
    }

    async fn complete_inner(&self, id: JobId, epoch: i64, now: Option<i64>) -> Result<()> {
        let mut tx = self.store.begin_write().await?;
        let now = clock(&mut tx, now).await?;
        complete_on(&mut tx, id, epoch, now).await?;
        tx.commit().await.map_err(map_db_error)
    }

    /// Fence and mark the job `done` inside the caller's write transaction `tx`. **The preferred
    /// pattern**: do the database effect on `tx`, call `complete_in(&mut tx, ..)?`, commit. The
    /// effect and the completion commit together or not at all, so the effect happens exactly
    /// once; a zombie's call fails with `Conflict("stale job epoch")` and the caller's `?`
    /// drops `tx`, rolling the effect back. See the module docs for the contract.
    pub async fn complete_in(tx: &mut SqliteTx, id: JobId, epoch: i64) -> Result<()> {
        let conn = tx.conn();
        let now = db_now(conn).await?;
        complete_on(conn, id, epoch, now).await
    }

    /// [`complete_in`](Self::complete_in) at time `now`.
    #[doc(hidden)]
    pub async fn complete_in_at(tx: &mut SqliteTx, id: JobId, epoch: i64, now: i64) -> Result<()> {
        complete_on(tx.conn(), id, epoch, now).await
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
        let mut tx = self.store.begin_write().await?;
        let now = clock(&mut tx, now).await?;
        let n = sqlx::query(
            "UPDATE jobs SET \
                    state = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'queued' END, \
                    run_after = CASE WHEN attempts >= max_attempts THEN run_after ELSE ? END, \
                    lease_owner = NULL, lease_expires_at = NULL, last_error = ?, updated_at = ? \
             WHERE id = ? AND epoch = ? AND state = 'running' AND lease_expires_at > ?",
        )
        .bind(now.saturating_add(retry))
        .bind(&error)
        .bind(now)
        .bind(id.0)
        .bind(epoch)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?
        .rows_affected();
        require_one(n)?;
        tx.commit().await.map_err(map_db_error)
    }

    /// Verify, inside the caller's write transaction `tx`, that job `id` is `running` with this
    /// `epoch` and an unexpired lease; otherwise `Conflict("stale job epoch")` and the caller
    /// must drop (roll back) `tx`, normally by returning the error with `?`.
    ///
    /// A fence makes a zombie's effect roll back; it does **not** make the step exactly-once: a
    /// crash after the fenced commit but before `complete` reruns the job under a new epoch
    /// that passes the fence, so the step must be idempotent. To also make the completion
    /// atomic with the effect, finish with [`complete_in`](Self::complete_in) instead.
    pub async fn fence(tx: &mut SqliteTx, id: JobId, epoch: i64) -> Result<()> {
        Self::fence_inner(tx, id, epoch, None).await
    }

    /// [`fence`](Self::fence) at time `now`.
    #[doc(hidden)]
    pub async fn fence_at(tx: &mut SqliteTx, id: JobId, epoch: i64, now: i64) -> Result<()> {
        Self::fence_inner(tx, id, epoch, Some(now)).await
    }

    async fn fence_inner(tx: &mut SqliteTx, id: JobId, epoch: i64, now: Option<i64>) -> Result<()> {
        let conn = tx.conn();
        let now = clock(conn, now).await?;
        let ok: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM jobs \
             WHERE id = ? AND epoch = ? AND state = 'running' AND lease_expires_at > ?",
        )
        .bind(id.0)
        .bind(epoch)
        .bind(now)
        .fetch_optional(conn)
        .await
        .map_err(map_db_error)?;
        ok.map(|_| ()).ok_or_else(stale)
    }

    /// Delete `done`, `failed` and `dead` jobs with `updated_at < older_than` (unix seconds),
    /// at most [`PURGE_BATCH`] per transaction, until none are left. Returns the rows removed.
    pub async fn purge_finished(&self, older_than: i64) -> Result<u64> {
        let mut total = 0u64;
        loop {
            let mut tx = self.store.begin_write().await?;
            let n = sqlx::query(
                "DELETE FROM jobs WHERE rowid IN (\
                   SELECT rowid FROM jobs \
                   WHERE state IN ('done', 'failed', 'dead') AND updated_at < ? LIMIT ?)",
            )
            .bind(older_than)
            .bind(PURGE_BATCH)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
            tx.commit().await.map_err(map_db_error)?;
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
        let now: i64 = sqlx::query_scalar("SELECT unixepoch()")
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)?;
        self.purge_finished_at(now, JOB_RETENTION_SECS).await
    }
}
