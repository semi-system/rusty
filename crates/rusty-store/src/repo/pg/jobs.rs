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

/// Candidates fetched per probe round (see `next_candidate`).
const CLAIM_WINDOW: i64 = 32;
// The statements spell the window as a literal (a bound LIMIT makes generic plans assume 10%).
const _: () = assert!(CLAIM_WINDOW == 32);
/// Probe rounds per candidate search: with `CLAIM_WINDOW` this bounds a claim to 512 rows read
/// even if other claimers hold locks on all of them.
const CLAIM_MAX_ROUNDS: u32 = 16;

/// Step 1 of a claim: the next `CLAIM_WINDOW` eligible job ids in `(run_after, id)` order after
/// the key `($3, $4)`, WITHOUT locking, plus the decision time. Two ordered index probes
/// (review PG5 I3): queued rows by `jobs_queued_order (run_after, id) WHERE state = 'queued'`,
/// running rows with an expired lease by `jobs_running_lease (lease_expires_at) WHERE state =
/// 'running'` (the expired set is small: crashed workers, reaped by the next claims). The old
/// single disjunction could use neither index and read, sorted and filtered every job row
/// (61 ms per claim at 500k rows). `$2` = the supplied time of the `_at` twins, else the
/// database clock; the same value is bound into the later statements.
const CLAIM_PEEK: &str = "SELECT c.id, c.run_after, COALESCE($2, {NOW}) AS now FROM (\
      SELECT u.id, u.run_after FROM (\
        (SELECT id, run_after FROM jobs \
          WHERE state = 'queued' AND run_after <= COALESCE($2, {NOW}) AND kind = ANY($1) \
            AND (run_after, id) > ($3, $4) \
          ORDER BY run_after, id LIMIT 32) \
        UNION ALL \
        (SELECT id, run_after FROM jobs \
          WHERE state = 'running' AND lease_expires_at <= COALESCE($2, {NOW}) AND kind = ANY($1) \
            AND (run_after, id) > ($3, $4) \
          ORDER BY run_after, id LIMIT 32)\
      ) u ORDER BY u.run_after, u.id LIMIT 32\
    ) c ORDER BY c.run_after, c.id";

/// Step 2: lock the first of those ids that is still eligible (READ COMMITTED re-evaluates the
/// predicate on a row another claimer changed) and not locked by another claimer. Primary-key
/// probes only. `$1` = ids from step 1, `$2` = the decision time of step 1.
const CLAIM_LOCK: &str = "SELECT j.id, j.state, j.attempts, j.max_attempts FROM jobs j \
    WHERE j.id = ANY($1) \
      AND ((j.state = 'queued' AND j.run_after <= $2) \
        OR (j.state = 'running' AND j.lease_expires_at <= $2)) \
    ORDER BY j.run_after, j.id LIMIT 1 FOR UPDATE OF j SKIP LOCKED";

/// The two claim probe statements, rendered for Postgres, for the plan tests.
#[cfg(feature = "test-knobs")]
pub fn claim_statements_for_test() -> (String, String) {
    (r(CLAIM_PEEK), r(CLAIM_LOCK))
}

struct Candidate {
    id: i64,
    state: String,
    attempts: i64,
    max_attempts: i64,
    now: i64,
}

/// The first eligible job in `(run_after, id)` order that no other transaction has locked, now
/// locked by this one: `ORDER BY .. LIMIT 1 FOR UPDATE SKIP LOCKED` semantics over the whole
/// queue, with every round bounded by index probes. A round whose window was full but yielded
/// nothing lockable (every row held by other claimers) continues after its last key.
async fn next_candidate(
    c: &mut sqlx::PgConnection,
    kinds: &[&str],
    now: Option<i64>,
) -> std::result::Result<Option<Candidate>, sqlx::Error> {
    let mut after = (i64::MIN, i64::MIN);
    for _ in 0..CLAIM_MAX_ROUNDS {
        let window = sqlx::query(&r(CLAIM_PEEK))
            .bind(kinds)
            .bind(now)
            .bind(after.0)
            .bind(after.1)
            .fetch_all(&mut *c)
            .await?;
        let (Some(first), Some(last)) = (window.first(), window.last()) else {
            return Ok(None);
        };
        let decided: i64 = first.try_get("now")?;
        let ids: Vec<i64> = window.iter().map(|w| w.try_get("id")).collect::<std::result::Result<_, sqlx::Error>>()?;
        if let Some(row) = sqlx::query(&crate::repo::pg_lit(CLAIM_LOCK))
            .bind(&ids)
            .bind(decided)
            .fetch_optional(&mut *c)
            .await?
        {
            return Ok(Some(Candidate {
                id: row.try_get("id")?,
                state: row.try_get("state")?,
                attempts: row.try_get("attempts")?,
                max_attempts: row.try_get("max_attempts")?,
                now: decided,
            }));
        }
        if (window.len() as i64) < CLAIM_WINDOW {
            return Ok(None);
        }
        after = (last.try_get("run_after")?, last.try_get("id")?);
    }
    Ok(None)
}

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
                            &crate::repo::pg_lit("SELECT id FROM domains WHERE id = $1 FOR NO KEY UPDATE"),
                        )
                        .bind(d.0)
                        .fetch_optional(&mut *c)
                        .await?;
                    }
                    None => {
                        sqlx::query(&crate::repo::pg_lit("SELECT pg_advisory_xact_lock($1, $2)"))
                            .bind(NO_DOMAIN_LOCK_CLASS)
                            .bind(NO_DOMAIN_LOCK_KEY)
                            .execute(&mut *c)
                            .await?;
                    }
                }
                let pending: i64 = match job.domain {
                    Some(d) => {
                        sqlx::query_scalar(
                            &crate::repo::pg_lit("SELECT count(*) FROM jobs WHERE domain_id = $1 \
                             AND state IN ('queued', 'running')"),
                        )
                        .bind(d.0)
                        .fetch_one(&mut *c)
                        .await?
                    }
                    None => {
                        sqlx::query_scalar(
                            &crate::repo::pg_lit("SELECT count(*) FROM jobs WHERE domain_id IS NULL \
                             AND state IN ('queued', 'running')"),
                        )
                        .fetch_one(&mut *c)
                        .await?
                    }
                };
                if pending >= i64::from(job.effective_cap()) {
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
                    let Some(cand) = next_candidate(&mut *c, &kinds, req.now).await? else {
                        break Ok(None);
                    };
                    let Candidate { id, state, attempts, max_attempts, now } = cand;
                    if attempts >= max_attempts {
                        sqlx::query(&crate::repo::pg_lit(sql::SETTLE_DEAD))
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
                            sqlx::query(&crate::repo::pg_lit(sql::SETTLE_DEAD))
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
                    &crate::repo::pg_lit("DELETE FROM jobs WHERE id IN (\
                       SELECT id FROM jobs \
                       WHERE state IN ('done', 'failed', 'dead') AND updated_at < $1 \
                       ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED)"),
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
