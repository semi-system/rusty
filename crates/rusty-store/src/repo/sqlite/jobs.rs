//! `JobRepo` on SQLite: `BEGIN IMMEDIATE` is the global write lock, so the claim walk and the
//! cap count need no row locks.

use async_trait::async_trait;
use rusty_core::{DomainId, Error, JobId, Result};
use sqlx::{Row, SqliteConnection};

use super::SqliteRepo;
use crate::jobs::{parse_payload, ClaimedJob, MAX_SETTLED_PER_CLAIM};
use crate::repo::jobs::{exhausted_reason, require_one, sql, ClaimRequest, NewJob};
use crate::repo::{render_sqlite as r, JobFence, JobRepo};
use crate::map_db_error;

async fn clock(conn: &mut SqliteConnection, now: Option<i64>) -> Result<i64> {
    match now {
        Some(n) => Ok(n),
        None => sqlx::query_scalar(&crate::repo::sqlite_lit("SELECT unixepoch()"))
            .fetch_one(conn)
            .await
            .map_err(map_db_error),
    }
}

async fn settle_dead(conn: &mut SqliteConnection, id: i64, why: &str, now: i64) -> Result<()> {
    sqlx::query(&r(sql::SETTLE_DEAD))
        .bind(id)
        .bind(why)
        .bind(now)
        .execute(conn)
        .await
        .map_err(map_db_error)?;
    Ok(())
}

#[async_trait]
impl JobRepo for SqliteRepo {
    async fn db_now(&self) -> Result<i64> {
        sqlx::query_scalar(&crate::repo::sqlite_lit("SELECT unixepoch()"))
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)
    }

    async fn enqueue(&self, job: &NewJob<'_>) -> Result<JobId> {
        let mut tx = self.store.begin_write().await?;
        let now = clock(&mut tx, job.now).await?;
        // The count and the insert share one write transaction: concurrent enqueues cannot both
        // slip under the cap.
        let pending: i64 = match job.domain {
            Some(d) => sqlx::query_scalar(
                &crate::repo::sqlite_lit("SELECT count(*) FROM jobs WHERE domain_id = ? AND state IN ('queued', 'running')"),
            )
            .bind(d.0)
            .fetch_one(&mut *tx)
            .await,
            None => sqlx::query_scalar(
                &crate::repo::sqlite_lit("SELECT count(*) FROM jobs WHERE domain_id IS NULL AND state IN ('queued', 'running')"),
            )
            .fetch_one(&mut *tx)
            .await,
        }
        .map_err(map_db_error)?;
        if pending >= i64::from(job.effective_cap()) {
            return Err(Error::Quota("job queue full".into()));
        }
        let id: i64 = sqlx::query_scalar(
            &crate::repo::sqlite_lit("INSERT INTO jobs (kind, domain_id, state, payload, attempts, max_attempts, \
                               run_after, epoch, created_at, updated_at) \
             VALUES (?, ?, 'queued', ?, 0, ?, ?, 0, ?, ?) RETURNING id"),
        )
        .bind(job.kind)
        .bind(job.domain.map(|d: DomainId| d.0))
        .bind(job.payload)
        .bind(i64::from(job.max_attempts))
        .bind(job.run_after.unwrap_or(now))
        .bind(now)
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(JobId(id))
    }

    async fn claim(&self, req: &ClaimRequest<'_>) -> Result<Option<ClaimedJob>> {
        // Only `?` placeholders are interpolated; every kind is bound.
        let placeholders = vec!["?"; req.kinds.len()].join(", ");
        let select = format!(
            "SELECT id, state, attempts, max_attempts FROM jobs WHERE kind IN ({placeholders}) \
             AND ((state = 'queued' AND run_after <= ?) \
               OR (state = 'running' AND lease_expires_at <= ?)) \
             ORDER BY run_after, id LIMIT 1"
        );
        let mut tx = self.store.begin_write().await?;
        let now = clock(&mut tx, req.now).await?;
        let mut settled = 0u32;
        let claimed = loop {
            if settled >= MAX_SETTLED_PER_CLAIM {
                break None;
            }
            let mut q = sqlx::query(&select);
            for k in req.kinds {
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
                settle_dead(&mut tx, id, exhausted_reason(state == "running"), now).await?;
                settled += 1;
                continue;
            }
            let row = sqlx::query(&r(sql::CLAIM_UPDATE))
                .bind(id)
                .bind(req.worker)
                .bind(now.saturating_add(i64::from(req.lease_secs)))
                .bind(now)
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
                        created_at: row.try_get("created_at").map_err(map_db_error)?,
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

    async fn heartbeat(&self, id: JobId, epoch: i64, lease_secs: u32, now: Option<i64>) -> Result<()> {
        let mut tx = self.store.begin_write().await?;
        let n = sqlx::query(&r(sql::HEARTBEAT))
            .bind(id.0)
            .bind(epoch)
            .bind(i64::from(lease_secs))
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
        require_one(n)?;
        tx.commit().await.map_err(map_db_error)
    }

    async fn complete(&self, id: JobId, epoch: i64, now: Option<i64>) -> Result<()> {
        let mut tx = self.store.begin_write().await?;
        let n = sqlx::query(&r(sql::COMPLETE))
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

    async fn fail(&self, id: JobId, epoch: i64, error: &str, retry_secs: i64, now: Option<i64>) -> Result<()> {
        let mut tx = self.store.begin_write().await?;
        let n = sqlx::query(&r(sql::FAIL))
            .bind(id.0)
            .bind(epoch)
            .bind(error)
            .bind(retry_secs)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
        require_one(n)?;
        tx.commit().await.map_err(map_db_error)
    }

    async fn reschedule(
        &self,
        id: JobId,
        epoch: i64,
        payload: &str,
        error: &str,
        retry_secs: i64,
        now: Option<i64>,
    ) -> Result<()> {
        let mut tx = self.store.begin_write().await?;
        let n = sqlx::query(&r(sql::RESCHEDULE))
            .bind(id.0)
            .bind(epoch)
            .bind(payload)
            .bind(error)
            .bind(retry_secs)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
        require_one(n)?;
        tx.commit().await.map_err(map_db_error)
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
        let mut tx = self.store.begin_write().await?;
        let n = sqlx::query(
            &crate::repo::sqlite_lit("DELETE FROM jobs WHERE rowid IN (\
               SELECT rowid FROM jobs \
               WHERE state IN ('done', 'failed', 'dead') AND updated_at < ? LIMIT ?)"),
        )
        .bind(older_than)
        .bind(limit)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?
        .rows_affected();
        tx.commit().await.map_err(map_db_error)?;
        Ok(n)
    }
}
