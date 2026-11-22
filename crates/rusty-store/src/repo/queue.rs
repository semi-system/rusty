//! `QueueRepo`: the outbound queue as the admin web manages it, and the delivery log (docs/design-queue.md). The jobs are the ones of `JobRepo` (kind `outbound`); this repository only reads
//! them for display and applies the four operator actions, each one limited to the jobs of one domain, in the states where it is safe: a job a worker has leased is never changed.

use async_trait::async_trait;
use rusty_core::{DomainId, Result};

/// A job that is held is given a `run_after` far in the future (the year 3000): no worker claims it, and it is shown as held.
pub const HELD_UNTIL: i64 = 32_503_680_000;

/// One queued job as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueJob {
    pub id: i64,
    pub state: String,
    pub attempts: i64,
    pub created_at: i64,
    pub run_after: i64,
    pub last_error: Option<String>,
    /// The job's payload as stored (a JSON object): the envelope sender, the recipients still to be served and the destination domain.
    pub payload: String,
}

/// One line of the delivery log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub id: i64,
    /// `out` (the outbound worker) or `in` (port 25).
    pub direction: String,
    pub at: i64,
    pub job_id: Option<i64>,
    pub sender: String,
    pub recipient: String,
    pub event: String,
    pub remote: Option<String>,
    pub status: Option<String>,
    pub detail: Option<String>,
}

/// A line about incoming mail: accepted for a mailbox of ours, or refused after DATA. The domain is the recipient's, found by the statement that writes it (a recipient of a domain we do not
/// host has no row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundRow {
    pub sender: String,
    pub recipient: String,
    pub event: &'static str,
    /// The address of the sending server.
    pub remote: String,
    pub status: Option<String>,
    pub detail: Option<String>,
}

/// A row the worker writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLogRow {
    pub domain: DomainId,
    pub job_id: Option<i64>,
    pub sender: String,
    pub recipient: String,
    pub event: &'static str,
    pub remote: Option<String>,
    pub status: Option<String>,
    pub detail: Option<String>,
}

/// How many log lines of one kind a day had: `day` is the number of the UTC day (seconds since the epoch divided by 86 400).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatRow {
    pub day: i64,
    pub direction: String,
    pub event: String,
    pub count: i64,
}

#[async_trait]
pub trait QueueRepo: Send + Sync {
    /// The log lines of a domain since `since` (unix seconds), counted per day, direction and event, oldest day first (the statistics page).
    async fn stats(&self, domain: DomainId, since: i64) -> Result<Vec<StatRow>>;
    /// The outbound jobs of a domain, newest first, from just below `before` (a job id; `None`: the newest). `state` is one of the job states; `held` selects queued jobs that are held, and
    /// `waiting` the other queued ones. `needle` (lower case) must occur in the payload: a recipient, the sender or the destination.
    async fn jobs(&self, domain: DomainId, state: Option<&str>, needle: Option<&str>, before: Option<i64>, limit: i64) -> Result<Vec<QueueJob>>;
    /// Hold a queued job (no worker will take it until it is released). `false`: no such queued job in this domain.
    async fn hold(&self, domain: DomainId, job: i64) -> Result<bool>;
    /// Make a queued job due now (releases a held one, or sends a deferred one without waiting for its time). `false`: no such queued job.
    async fn release(&self, domain: DomainId, job: i64) -> Result<bool>;
    /// Put a job that was given up (`dead` or `failed`) back in the queue with a fresh count of attempts. `false`: no such job.
    async fn requeue(&self, domain: DomainId, job: i64) -> Result<bool>;
    /// Delete a job that no worker holds (queued, failed or dead). `false`: none, or a worker holds it now.
    async fn delete(&self, domain: DomainId, job: i64) -> Result<bool>;
    /// The delivery log of a domain, newest first, from just below `before`; `needle` (lower case) must occur in the sender or the recipient.
    async fn trace(&self, domain: DomainId, needle: Option<&str>, before: Option<i64>, limit: i64) -> Result<Vec<LogRow>>;
    /// Write rows of the log (the worker's; a failure of the log never fails a delivery, so the caller only logs it).
    async fn log(&self, rows: &[NewLogRow]) -> Result<()>;
    /// Write the lines about incoming mail (best effort, like [`log`](Self::log)).
    async fn log_inbound(&self, rows: &[InboundRow]) -> Result<()>;
    /// Remove log rows older than `older_than` (unix seconds), at most `limit`; how many.
    async fn purge_log(&self, older_than: i64, limit: i64) -> Result<u64>;
}

pub(crate) mod sql {
    pub const JOBS: &str = "SELECT id, state, attempts, created_at, run_after, last_error, payload FROM jobs \
        WHERE domain_id = $1 AND kind = 'outbound' AND id < COALESCE($4, CAST(9223372036854775807 AS BIGINT)) \
        AND (CAST($2 AS TEXT) IS NULL OR state = CAST($2 AS TEXT) OR (CAST($2 AS TEXT) = 'held' AND state = 'queued' AND run_after >= $6) \
             OR (CAST($2 AS TEXT) = 'waiting' AND state = 'queued' AND run_after < $6)) \
        AND (CAST($3 AS TEXT) IS NULL OR strpos(lower(payload), CAST($3 AS TEXT)) > 0) ORDER BY id DESC LIMIT $5";
    pub const HOLD: &str = "UPDATE jobs SET run_after = $3, updated_at = rs_now() WHERE id = $1 AND domain_id = $2 AND kind = 'outbound' AND state = 'queued'";
    pub const RELEASE: &str = "UPDATE jobs SET run_after = rs_now(), updated_at = rs_now() WHERE id = $1 AND domain_id = $2 AND kind = 'outbound' AND state = 'queued'";
    pub const REQUEUE: &str = "UPDATE jobs SET state = 'queued', attempts = 0, run_after = rs_now(), lease_owner = NULL, lease_expires_at = NULL, epoch = epoch + 1, updated_at = rs_now() \
        WHERE id = $1 AND domain_id = $2 AND kind = 'outbound' AND state IN ('dead', 'failed')";
    pub const DELETE: &str = "DELETE FROM jobs WHERE id = $1 AND domain_id = $2 AND kind = 'outbound' AND state IN ('queued', 'dead', 'failed')";
    pub const TRACE: &str = "SELECT id, direction, at, job_id, sender, recipient, event, remote, status, detail FROM mail_log \
        WHERE domain_id = $1 AND id < COALESCE($3, CAST(9223372036854775807 AS BIGINT)) \
        AND (CAST($2 AS TEXT) IS NULL OR strpos(lower(recipient), CAST($2 AS TEXT)) > 0 OR strpos(lower(sender), CAST($2 AS TEXT)) > 0) ORDER BY id DESC LIMIT $4";
    pub const LOG: &str = "INSERT INTO mail_log (at, domain_id, job_id, sender, recipient, event, remote, status, detail) \
        SELECT rs_now(), x.d, x.j, x.s, x.r, x.e, x.m, x.t, x.n FROM unnest($1::bigint[], $2::bigint[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[], $8::text[]) AS x(d, j, s, r, e, m, t, n)";
    pub const LOG_INBOUND: &str = "INSERT INTO mail_log (at, domain_id, direction, sender, recipient, event, remote, status, detail) \
        SELECT rs_now(), d.id, 'in', x.s, x.r, x.e, x.m, x.t, x.n FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[]) AS x(s, r, e, m, t, n) \
        JOIN domains d ON d.name = lower(split_part(x.r, '@', 2))";
    pub const STATS: &str = "SELECT at / 86400, direction, event, count(*) FROM mail_log WHERE domain_id = $1 AND at >= $2 GROUP BY 1, 2, 3 ORDER BY 1, 2, 3";
    pub const PURGE_LOG: &str = "DELETE FROM mail_log WHERE id IN (SELECT id FROM mail_log WHERE at < $1 ORDER BY id LIMIT $2)";
}
