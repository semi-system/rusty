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
//! re-claims, and the job is simply claimed again (at-least-once; side effects are fenced).
//!
//! A job out of attempts (`attempts >= max_attempts`, at most 20) becomes `dead`, either when
//! `fail` is called on its last attempt or when its lease lapses on it (the next `claim`
//! reaps it). `dead` and `done` rows are removed by [`purge_finished`](JobQueue::purge_finished).
//!
//! # Fencing
//!
//! [`JobQueue::fence`] checks, inside the caller's own write transaction, that the job is still
//! `running` with this epoch and an unexpired lease. A worker does its database side effect and
//! the fence check in one transaction, so the effect commits if and only if the worker still
//! owned the job at that moment; a zombie's transaction fails the fence and rolls back. Side
//! effects outside the database (files, mail) must be idempotent or made visible only by such a
//! transaction.
//!
//! # Time
//!
//! Every decision uses the database clock (`unixepoch()`), read inside the same transaction as
//! the decision. Each time-dependent method has an `_at(.., now)` twin taking unix seconds so
//! tests never sleep; production code calls the plain method.
//!
//! # Hostile input
//!
//! `kind` is `[a-z0-9._-]`, 1..=64 bytes. The payload is serialized, limited to 64 KiB and must
//! parse back (so a value nested deeper than the parser accepts is refused at enqueue and can
//! not poison `claim`; if a row nevertheless holds an unparseable payload, `claim` marks it
//! `dead` and carries on). The `last_error` a worker supplies is data: control and
//! bidi/format characters are removed and it is truncated to 512 bytes. Error messages are
//! fixed strings and never contain caller input.
//!
//! # Postgres port
//!
//! `claim` is one `BEGIN IMMEDIATE` transaction on SQLite: reap exhausted jobs, `SELECT` the
//! first eligible row, `UPDATE` it. On Postgres the same shape maps 1:1 to one transaction in
//! which the `SELECT ... ORDER BY run_after, id LIMIT 1` gains `FOR UPDATE SKIP LOCKED` (so
//! concurrent claimers skip rows another transaction is claiming instead of blocking), and
//! `unixepoch()` becomes `extract(epoch FROM now())` evaluated by the database (never the
//! client clock). `fence` additionally locks the job row (`SELECT ... FOR SHARE`) on Postgres,
//! where SQLite's single writer lock already serializes it against `claim`.

use rusty_core::{DomainId, Error, JobId, Result};
use serde_json::Value;
use sqlx::{Row, SqliteConnection};

use crate::gc::PURGE_BATCH;
use crate::{map_db_error, SqliteStore, SqliteTx};

/// Largest serialized job payload, in bytes.
pub const MAX_JOB_PAYLOAD_BYTES: usize = 65536;
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

/// Remove control, line/paragraph separator, bidi and other format characters, then cut to
/// [`MAX_LAST_ERROR_BYTES`] on a character boundary.
fn sanitize_error(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let bad = c.is_control()
            || matches!(
                c,
                '\u{200B}'..='\u{200F}'
                    | '\u{2028}'..='\u{202E}'
                    | '\u{2060}'..='\u{2064}'
                    | '\u{2066}'..='\u{206F}'
                    | '\u{FEFF}'
                    | '\u{FFF9}'..='\u{FFFB}'
            );
        if bad {
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

impl JobQueue {
    pub fn new(store: SqliteStore) -> JobQueue {
        JobQueue { store }
    }

    /// Queue a job with the default attempt cap ([`DEFAULT_JOB_ATTEMPTS`]). `run_after`
    /// (unix seconds) defaults to now. Unknown/oversized `kind` or `payload` is `Invalid`.
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
        let text = serde_json::to_string(payload).map_err(|_| invalid("invalid job payload"))?;
        if text.len() > MAX_JOB_PAYLOAD_BYTES {
            return Err(invalid("job payload too large"));
        }
        // Must parse back, or a claim could never hand it to a worker.
        if serde_json::from_str::<Value>(&text).is_err() {
            return Err(invalid("invalid job payload"));
        }
        let mut tx = self.store.begin_write().await?;
        let now = clock(&mut tx, now).await?;
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
    /// (`1..=`[`MAX_LEASE_SECS`]), or `None` if there is none. The whole claim is one write
    /// transaction (see the module docs for the Postgres mapping).
    pub async fn claim(
        &self,
        worker: &str,
        kinds: &[&str],
        lease_secs: u32,
    ) -> Result<Option<ClaimedJob>> {
        self.claim_inner(worker, kinds, lease_secs, None).await
    }

    /// [`claim`](Self::claim) at time `now`.
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
            "SELECT id FROM jobs WHERE kind IN ({placeholders}) \
             AND ((state = 'queued' AND run_after <= ?) \
               OR (state = 'running' AND lease_expires_at <= ?)) \
             ORDER BY run_after, id LIMIT 1"
        );

        let mut tx = self.store.begin_write().await?;
        let now = clock(&mut tx, now).await?;
        // Reap jobs that have no attempt left: a lapsed lease on the last attempt (or a
        // queued row that somehow has none left) ends the job instead of being claimed.
        sqlx::query(
            "UPDATE jobs SET state = 'dead', lease_owner = NULL, lease_expires_at = NULL, \
                    last_error = 'lease expired on the last attempt', updated_at = ? \
             WHERE attempts >= max_attempts \
               AND ((state = 'running' AND lease_expires_at <= ?) OR state = 'queued')",
        )
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?;

        let claimed = loop {
            let mut q = sqlx::query_scalar::<_, i64>(&select);
            for k in kinds {
                q = q.bind(*k);
            }
            let Some(id) = q
                .bind(now)
                .bind(now)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_db_error)?
            else {
                break None;
            };
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
            match serde_json::from_str::<Value>(&text) {
                Ok(payload) => {
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
                Err(_) => {
                    // A row that can never be handed to a worker: park it and keep going.
                    sqlx::query(
                        "UPDATE jobs SET state = 'dead', lease_owner = NULL, \
                                lease_expires_at = NULL, last_error = 'unreadable payload' \
                         WHERE id = ?",
                    )
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(map_db_error)?;
                }
            }
        };
        tx.commit().await.map_err(map_db_error)?;
        Ok(claimed)
    }

    /// Extend the lease to `now + lease_secs`. `Conflict("stale job epoch")` unless the job is
    /// `running` with this epoch and an unexpired lease.
    pub async fn heartbeat(&self, id: JobId, epoch: i64, lease_secs: u32) -> Result<()> {
        self.heartbeat_inner(id, epoch, lease_secs, None).await
    }

    /// [`heartbeat`](Self::heartbeat) at time `now`.
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
    pub async fn complete(&self, id: JobId, epoch: i64) -> Result<()> {
        self.complete_inner(id, epoch, None).await
    }

    /// [`complete`](Self::complete) at time `now`.
    pub async fn complete_at(&self, id: JobId, epoch: i64, now: i64) -> Result<()> {
        self.complete_inner(id, epoch, Some(now)).await
    }

    async fn complete_inner(&self, id: JobId, epoch: i64, now: Option<i64>) -> Result<()> {
        let mut tx = self.store.begin_write().await?;
        let now = clock(&mut tx, now).await?;
        let n = sqlx::query(
            "UPDATE jobs SET state = 'done', lease_owner = NULL, lease_expires_at = NULL, \
                    updated_at = ? \
             WHERE id = ? AND epoch = ? AND state = 'running' AND lease_expires_at > ?",
        )
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
    /// must drop (roll back) `tx`. Do the side effect and this check in the same transaction.
    pub async fn fence(tx: &mut SqliteTx, id: JobId, epoch: i64) -> Result<()> {
        Self::fence_inner(tx, id, epoch, None).await
    }

    /// [`fence`](Self::fence) at time `now`.
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

    /// Delete `done` and `dead` jobs with `updated_at < older_than` (unix seconds), at most
    /// [`PURGE_BATCH`] per transaction, until none are left. Returns the rows removed.
    pub async fn purge_finished(&self, older_than: i64) -> Result<u64> {
        let mut total = 0u64;
        loop {
            let mut tx = self.store.begin_write().await?;
            let n = sqlx::query(
                "DELETE FROM jobs WHERE rowid IN (\
                   SELECT rowid FROM jobs \
                   WHERE state IN ('done', 'dead') AND updated_at < ? LIMIT ?)",
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
}
