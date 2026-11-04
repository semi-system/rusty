//! `JobRepo` on Postgres (READ COMMITTED). No mailbox lock: job rows only (see `repo/jobs.rs`).
//! No session state: the cap serialisation lock is transaction-scoped.

use async_trait::async_trait;
use rusty_core::{DomainId, Error, JobId, Result};
use sqlx::Row;

use super::tx::tx_retry;
use super::PgRepo;
use crate::jobs::{parse_payload, ClaimedJob, MAX_SETTLED_PER_CLAIM};
use crate::map_db_error;
use crate::repo::jobs::{exhausted_reason, require_one, sql, ClaimRequest, NewJob};
use crate::repo::{render_pg as r, JobFence, JobRepo};

/// Class of the transaction-scoped advisory lock that serialises `enqueue` for jobs without a
/// domain (the two-integer form keeps it apart from any other advisory use).
const NO_DOMAIN_LOCK_CLASS: i32 = 0x4A4F_4253; // "JOBS"
const NO_DOMAIN_LOCK_KEY: i32 = 1;

const CLAIM_SELECT: &str = "SELECT j.id, j.state, j.attempts, j.max_attempts, t.now \
    FROM jobs j CROSS JOIN (SELECT COALESCE($2, {NOW}) AS now) t \
    WHERE j.kind = ANY($1) \
      AND ((j.state = 'queued' AND j.run_after <= t.now) \
        OR (j.state = 'running' AND j.lease_expires_at <= t.now)) \
    ORDER BY j.run_after, j.id LIMIT 1 FOR UPDATE OF j SKIP LOCKED";

#[async_trait]
impl JobRepo for PgRepo {
    async fn db_now(&self) -> Result<i64> {
        sqlx::query_scalar(&r("SELECT {NOW}"))
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)
    }

    async fn enqueue(&self, job: &NewJob<'_>) -> Result<JobId> {
        let id = tx_retry!(self, None, |c| {
            {
                // Serialise the count-then-insert per scope (the SQLite single writer does this
                // implicitly). A missing domain is left to the foreign key (Conflict).
                match job.domain {
                    Some(d) => {
                        sqlx::query_scalar::<_, i64>(
                            "SELECT id FROM domains WHERE id = $1 FOR NO KEY UPDATE",
                        )
                        .bind(d.0)
                        .fetch_optional(&mut *c)
                        .await?;
                    }
                    None => {
                        sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
                            .bind(NO_DOMAIN_LOCK_CLASS)
                            .bind(NO_DOMAIN_LOCK_KEY)
                            .execute(&mut *c)
                            .await?;
                    }
                }
                let pending: i64 = match job.domain {
                    Some(d) => {
                        sqlx::query_scalar(
                            "SELECT count(*) FROM jobs WHERE domain_id = $1 \
                             AND state IN ('queued', 'running')",
                        )
                        .bind(d.0)
                        .fetch_one(&mut *c)
                        .await?
                    }
                    None => {
                        sqlx::query_scalar(
                            "SELECT count(*) FROM jobs WHERE domain_id IS NULL \
                             AND state IN ('queued', 'running')",
                        )
                        .fetch_one(&mut *c)
                        .await?
                    }
                };
                if pending >= i64::from(job.cap) {
                    return Err(Error::Quota("job queue full".into()).into());
                }
                let id: i64 = sqlx::query_scalar(&r(
                    "INSERT INTO jobs (kind, domain_id, state, payload, attempts, max_attempts, \
                                       run_after, epoch, created_at, updated_at) \
                     SELECT $1, $2, 'queued', $3, 0, $4, COALESCE($5, t.now), 0, t.now, t.now \
                     FROM (SELECT COALESCE($6, {NOW}) AS now) t RETURNING id",
                ))
                .bind(job.kind)
                .bind(job.domain.map(|d: DomainId| d.0))
                .bind(job.payload)
                .bind(i64::from(job.max_attempts))
                .bind(job.run_after)
                .bind(job.now)
                .fetch_one(&mut *c)
                .await?;
                Ok(id)
            }
        })?;
        Ok(JobId(id))
    }

    async fn claim(&self, req: &ClaimRequest<'_>) -> Result<Option<ClaimedJob>> {
        let kinds: Vec<&str> = req.kinds.to_vec();
        let claimed = tx_retry!(self, None, |c| {
            {
                let mut settled = 0u32;
                loop {
                    if settled >= MAX_SETTLED_PER_CLAIM {
                        break Ok(None);
                    }
                    let Some(cand) = sqlx::query(&r(CLAIM_SELECT))
                        .bind(&kinds)
                        .bind(req.now)
                        .fetch_optional(&mut *c)
                        .await?
                    else {
                        break Ok(None);
                    };
                    let id: i64 = cand.try_get("id")?;
                    let state: String = cand.try_get("state")?;
                    let attempts: i64 = cand.try_get("attempts")?;
                    let max_attempts: i64 = cand.try_get("max_attempts")?;
                    let now: i64 = cand.try_get("now")?;
                    if attempts >= max_attempts {
                        sqlx::query(sql::SETTLE_DEAD)
                            .bind(id)
                            .bind(exhausted_reason(state == "running"))
                            .bind(now)
                            .execute(&mut *c)
                            .await?;
                        settled += 1;
                        continue;
                    }
                    let row = sqlx::query(sql::CLAIM_UPDATE)
                        .bind(id)
                        .bind(req.worker)
                        .bind(now.saturating_add(i64::from(req.lease_secs)))
                        .bind(now)
                        .fetch_one(&mut *c)
                        .await?;
                    let text: String = row.try_get("payload")?;
                    let attempts: i64 = row.try_get("attempts")?;
                    match parse_payload(&text) {
                        Some(payload) => {
                            let attempts = u32::try_from(attempts)
                                .map_err(|_| Error::storage("job attempts out of range"))?;
                            break Ok(Some(ClaimedJob {
                                id: JobId(row.try_get("id")?),
                                kind: row.try_get("kind")?,
                                domain: row.try_get::<Option<i64>, _>("domain_id")?.map(DomainId),
                                payload,
                                epoch: row.try_get("epoch")?,
                                attempts,
                            }));
                        }
                        None => {
                            // A row that can never be handed to a worker: park it, keep going.
                            sqlx::query(sql::SETTLE_DEAD)
                                .bind(id)
                                .bind("unreadable payload")
                                .bind(now)
                                .execute(&mut *c)
                                .await?;
                            settled += 1;
                        }
                    }
                }
            }
        })?;
        Ok(claimed)
    }

    async fn heartbeat(&self, id: JobId, epoch: i64, lease_secs: u32, now: Option<i64>) -> Result<()> {
        let n = tx_retry!(self, None, |c| {
            {
                Ok(sqlx::query(&r(sql::HEARTBEAT))
                    .bind(id.0)
                    .bind(epoch)
                    .bind(i64::from(lease_secs))
                    .bind(now)
                    .execute(&mut *c)
                    .await?
                    .rows_affected())
            }
        })?;
        require_one(n)
    }

    async fn complete(&self, id: JobId, epoch: i64, now: Option<i64>) -> Result<()> {
        let n = tx_retry!(self, None, |c| {
            {
                Ok(sqlx::query(&r(sql::COMPLETE))
                    .bind(id.0)
                    .bind(epoch)
                    .bind(now)
                    .execute(&mut *c)
                    .await?
                    .rows_affected())
            }
        })?;
        require_one(n)
    }

    async fn fail(&self, id: JobId, epoch: i64, error: &str, retry_secs: i64, now: Option<i64>) -> Result<()> {
        let n = tx_retry!(self, None, |c| {
            {
                Ok(sqlx::query(&r(sql::FAIL))
                    .bind(id.0)
                    .bind(epoch)
                    .bind(error)
                    .bind(retry_secs)
                    .bind(now)
                    .execute(&mut *c)
                    .await?
                    .rows_affected())
            }
        })?;
        require_one(n)
    }

    async fn fence_check(&self, fence: JobFence, now: Option<i64>) -> Result<()> {
        let ok: Option<i64> = sqlx::query_scalar(&r(sql::FENCE))
            .bind(fence.id.0)
            .bind(fence.epoch)
            .bind(now)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?;
        ok.map(|_| ()).ok_or_else(crate::jobs::stale)
    }

    async fn purge_finished_batch(&self, older_than: i64, limit: i64) -> Result<u64> {
        let n = tx_retry!(self, None, |c| {
            {
                Ok(sqlx::query(
                    "DELETE FROM jobs WHERE id IN (\
                       SELECT id FROM jobs \
                       WHERE state IN ('done', 'failed', 'dead') AND updated_at < $1 \
                       ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED)",
                )
                .bind(older_than)
                .bind(limit)
                .execute(&mut *c)
                .await?
                .rows_affected())
            }
        })?;
        Ok(n)
    }
}
