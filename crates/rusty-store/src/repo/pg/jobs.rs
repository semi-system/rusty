//! `JobRepo` on Postgres (READ COMMITTED). No mailbox lock: job rows only (see `repo/jobs.rs`).
//! No session state: the cap serialisation lock is transaction-scoped.

use async_trait::async_trait;
use rusty_core::{DomainId, Error, JobId, Result};
use sqlx::Row;

use super::tx::tx_retry;
use super::PgRepo;
use crate::jobs::{parse_payload, ClaimedJob, MAX_SETTLED_PER_CLAIM};
use crate::map_db_error;
use crate::repo::jobs::{exhausted_reason, require_one, sql, BatchRequest, ClaimRequest, NewJob};
use crate::repo::{render_pg as r, JobFence, JobRepo};

/// Class of the transaction-scoped advisory lock that serialises `enqueue` for jobs without a
/// domain (the two-integer form keeps it apart from any other advisory use).
const NO_DOMAIN_LOCK_CLASS: i32 = 0x4A4F_4253; // "JOBS"
const NO_DOMAIN_LOCK_KEY: i32 = 1;

/// Candidates fetched per claim (see `CLAIM_PEEK`).
const CLAIM_WINDOW: i64 = 32;
// The statements spell the window as a literal (a bound LIMIT makes generic plans assume 10%).
const _: () = assert!(CLAIM_WINDOW == 32);
/// Step 1 of a claim: up to `CLAIM_WINDOW` eligible job ids in CLAIM ORDER, WITHOUT locking, plus the decision time (docs/design-fair-claim.md). The order is: a running job whose lease
/// expired (a worker that died: oldest first), then the queued jobs by their place in their own tenant's line (the oldest of every tenant first, then the second oldest of every tenant,
/// ...), and inside a place the tenant served least recently first (a tenant never served first). So a tenant with a large backlog gets one job per round like every other, and when
/// the first job of a tenant is held by another claimer the next claimer takes the next job, of the same tenant if no other has a job due.
/// `$3` is how many jobs of each tenant the window starts behind: round 0 starts at 0 and a round in which every job of the window is held by other claimers asks again `CLAIM_DEPTH`
/// deeper. The tenants are found by a skip scan on `jobs_queued_scope` (one index probe per tenant that has queued jobs), the jobs of a tenant by one probe of the same index, the
/// expired leases by `jobs_running_lease`. `$2` = the supplied time of the `_at` twins, else the database clock; the same value is bound into the later statements.
const CLAIM_PEEK: &str = "WITH RECURSIVE s(scope) AS (\
      (SELECT COALESCE(domain_id, 0) FROM jobs WHERE state = 'queued' ORDER BY COALESCE(domain_id, 0) LIMIT 1) \
      UNION ALL \
      SELECT (SELECT COALESCE(j.domain_id, 0) FROM jobs j WHERE j.state = 'queued' AND COALESCE(j.domain_id, 0) > s.scope \
              ORDER BY COALESCE(j.domain_id, 0) LIMIT 1) FROM s WHERE s.scope IS NOT NULL) \
    SELECT c.id, c.run_after, COALESCE($2, {NOW}) AS now FROM (\
      SELECT u.id, u.run_after, u.prio, u.place, u.turn FROM (\
        (SELECT id, run_after, 0 AS prio, CAST(0 AS BIGINT) AS place, CAST(0 AS BIGINT) AS turn FROM jobs \
          WHERE state = 'running' AND lease_expires_at <= COALESCE($2, {NOW}) AND kind = ANY($1) AND $3 = 0 \
          ORDER BY run_after, id LIMIT 32) \
        UNION ALL \
        (SELECT h.id, h.run_after, 1 AS prio, h.place, COALESCE(t.last_claim, 0) AS turn FROM s \
          CROSS JOIN LATERAL (SELECT q.id, q.run_after, q.kind, row_number() OVER (ORDER BY q.run_after, q.id) AS place FROM (\
              SELECT id, run_after, kind FROM jobs \
              WHERE state = 'queued' AND COALESCE(domain_id, 0) = s.scope AND run_after <= COALESCE($2, {NOW}) AND kind = ANY($1) \
              ORDER BY run_after, id OFFSET $3 LIMIT 8) q) h \
          LEFT JOIN job_turns t ON t.scope = s.scope AND t.kind = h.kind \
          WHERE s.scope IS NOT NULL \
          ORDER BY h.place, COALESCE(t.last_claim, 0), h.run_after, h.id LIMIT 32)\
      ) u ORDER BY u.prio, u.place, u.turn, u.run_after, u.id LIMIT 32\
    ) c ORDER BY c.prio, c.place, c.turn, c.run_after, c.id";

/// Step 2: lock the first of those ids, in the order of the window, that is still eligible (READ COMMITTED re-evaluates the predicate on a row another claimer changed) and not locked by
/// another claimer. Primary-key probes only. `$1` = ids from step 1 in claim order, `$2` = the decision time of step 1.
const CLAIM_LOCK: &str = "SELECT j.id, j.state, j.attempts, j.max_attempts, COALESCE(j.domain_id, 0) AS scope, j.kind FROM jobs j \
    WHERE j.id = ANY($1) \
      AND ((j.state = 'queued' AND j.run_after <= $2) \
        OR (j.state = 'running' AND j.lease_expires_at <= $2)) \
    ORDER BY array_position($1, j.id) LIMIT 1 FOR UPDATE OF j SKIP LOCKED";

/// The two claim probe statements, rendered for Postgres, for the plan tests.
#[cfg(feature = "test-knobs")]
pub fn claim_statements_for_test() -> (String, String) {
    (crate::repo::render_text(CLAIM_PEEK), crate::repo::render_text(CLAIM_LOCK))
}

struct Candidate {
    id: i64,
    state: String,
    attempts: i64,
    max_attempts: i64,
    /// The tenant (the domain id, 0 for a job with none) and the kind: whose turn this claim uses up.
    scope: i64,
    kind: String,
    now: i64,
}

/// Jobs of one tenant the window holds per round, and how many rounds a claim makes: with `CLAIM_WINDOW` a claim reads at most `CLAIM_MAX_ROUNDS * CLAIM_DEPTH` jobs of a tenant even if
/// other claimers hold them all.
const CLAIM_DEPTH: i64 = 8;
const _: () = assert!(CLAIM_DEPTH == 8);
const CLAIM_MAX_ROUNDS: i64 = 16;

/// The first eligible job of the claim order (see `CLAIM_PEEK`) that no other transaction has locked, now locked by this one. A round whose jobs are all held by other claimers is followed by
/// one that starts `CLAIM_DEPTH` deeper in every tenant's line; a round with no job at all ends the search.
async fn next_candidate(
    c: &mut sqlx::PgConnection,
    kinds: &[&str],
    now: Option<i64>,
) -> std::result::Result<Option<Candidate>, sqlx::Error> {
    for round in 0..CLAIM_MAX_ROUNDS {
        let window = sqlx::query(r(CLAIM_PEEK)).bind(kinds).bind(now).bind(round * CLAIM_DEPTH).fetch_all(&mut *c).await?;
        let Some(first) = window.first() else {
            return Ok(None);
        };
        let decided: i64 = first.try_get("now")?;
        let ids: Vec<i64> = window.iter().map(|w| w.try_get("id")).collect::<std::result::Result<_, sqlx::Error>>()?;
        let row = sqlx::query(crate::repo::pg_lit(CLAIM_LOCK)).bind(&ids).bind(decided).fetch_optional(&mut *c).await?;
        if let Some(row) = row {
            return Ok(Some(Candidate {
                id: row.try_get("id")?,
                state: row.try_get("state")?,
                attempts: row.try_get("attempts")?,
                max_attempts: row.try_get("max_attempts")?,
                scope: row.try_get("scope")?,
                kind: row.try_get("kind")?,
                now: decided,
            }));
        }
    }
    Ok(None)
}

#[async_trait]
impl JobRepo for PgRepo {
    async fn db_now(&self) -> Result<i64> {
        sqlx::query_scalar(r("SELECT {NOW}"))
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)
    }

    async fn enqueue(&self, job: &NewJob<'_>) -> Result<JobId> {
        let id = tx_retry!(self, None, |c| {
            {
                // Serialise the count-then-insert per scope (nothing else serialises it). A missing domain is left to the foreign key (Conflict).
                match job.domain {
                    Some(d) => {
                        sqlx::query_scalar::<_, i64>(
                            crate::repo::pg_lit("SELECT id FROM domains WHERE id = $1 FOR NO KEY UPDATE"),
                        )
                        .bind(d.0)
                        .fetch_optional(&mut *c)
                        .await?;
                    }
                    None => {
                        sqlx::query(crate::repo::pg_lit("SELECT pg_advisory_xact_lock($1, $2)"))
                            .bind(NO_DOMAIN_LOCK_CLASS)
                            .bind(NO_DOMAIN_LOCK_KEY)
                            .execute(&mut *c)
                            .await?;
                    }
                }
                let pending: i64 = match job.domain {
                    Some(d) => {
                        sqlx::query_scalar(
                            crate::repo::pg_lit("SELECT count(*) FROM jobs WHERE domain_id = $1 \
                             AND state IN ('queued', 'running')"),
                        )
                        .bind(d.0)
                        .fetch_one(&mut *c)
                        .await?
                    }
                    None => {
                        sqlx::query_scalar(
                            crate::repo::pg_lit("SELECT count(*) FROM jobs WHERE domain_id IS NULL \
                             AND state IN ('queued', 'running')"),
                        )
                        .fetch_one(&mut *c)
                        .await?
                    }
                };
                if pending >= i64::from(job.effective_cap()) {
                    return Err(Error::Quota("job queue full".into()).into());
                }
                let id: i64 = sqlx::query_scalar(r(
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

    async fn enqueue_batch(&self, req: &BatchRequest<'_>) -> Result<Vec<JobId>> {
        let ids = tx_retry!(self, None, |c| {
            {
                // Serialise per scope, always in the same order (domains by id, then the owner), so two
                // batches cannot deadlock and the checks below are not raced.
                let mut domains: Vec<Option<i64>> = req.jobs.iter().map(|j| j.domain.map(|d: DomainId| d.0)).collect();
                domains.sort_unstable();
                domains.dedup();
                for d in &domains {
                    match d {
                        Some(d) => {
                            sqlx::query_scalar::<_, i64>(crate::repo::pg_lit("SELECT id FROM domains WHERE id = $1 FOR NO KEY UPDATE"))
                                .bind(*d)
                                .fetch_optional(&mut *c)
                                .await?;
                        }
                        None => {
                            sqlx::query(crate::repo::pg_lit("SELECT pg_advisory_xact_lock($1, $2)"))
                                .bind(NO_DOMAIN_LOCK_CLASS)
                                .bind(NO_DOMAIN_LOCK_KEY)
                                .execute(&mut *c)
                                .await?;
                        }
                    }
                }
                if let Some((owner, _)) = req.owner {
                    sqlx::query_scalar::<_, i64>(crate::repo::pg_lit("SELECT id FROM accounts WHERE id = $1 FOR NO KEY UPDATE"))
                        .bind(owner)
                        .fetch_optional(&mut *c)
                        .await?;
                }
                let now: i64 = match req.now {
                    Some(n) => n,
                    None => sqlx::query_scalar(r("SELECT {NOW}")).fetch_one(&mut *c).await?,
                };
                let mut known: Vec<Option<i64>> = Vec::with_capacity(req.jobs.len());
                for j in req.jobs {
                    let existing = match j.dedupe_key {
                        Some(k) => {
                            sqlx::query_scalar::<_, i64>(crate::repo::pg_lit("SELECT id FROM jobs WHERE dedupe_key = $1"))
                                .bind(k)
                                .fetch_optional(&mut *c)
                                .await?
                        }
                        None => None,
                    };
                    known.push(existing);
                }
                let new: Vec<&crate::repo::jobs::BatchJob<'_>> =
                    req.jobs.iter().zip(&known).filter(|(_, i)| i.is_none()).map(|(j, _)| j).collect();
                let mut scopes: Vec<(Option<i64>, i64)> = Vec::new();
                for j in &new {
                    let d = j.domain.map(|d: DomainId| d.0);
                    match scopes.iter_mut().find(|(k, _)| *k == d) {
                        Some((_, n)) => *n += 1,
                        None => scopes.push((d, 1)),
                    }
                }
                for (d, add) in scopes {
                    let pending: i64 = match d {
                        Some(d) => {
                            sqlx::query_scalar(crate::repo::pg_lit("SELECT count(*) FROM jobs WHERE domain_id = $1 AND state IN ('queued', 'running')"))
                                .bind(d)
                                .fetch_one(&mut *c)
                                .await?
                        }
                        None => {
                            sqlx::query_scalar(crate::repo::pg_lit("SELECT count(*) FROM jobs WHERE domain_id IS NULL AND state IN ('queued', 'running')"))
                                .fetch_one(&mut *c)
                                .await?
                        }
                    };
                    let cap = match d {
                        Some(_) => req.domain_cap.min(crate::jobs::MAX_PENDING_JOBS_PER_DOMAIN),
                        None => req.no_domain_cap.min(crate::jobs::MAX_PENDING_JOBS_GLOBAL_NO_DOMAIN),
                    };
                    if pending + add > i64::from(cap) {
                        return Err(Error::Quota("job queue full".into()).into());
                    }
                }
                if let (Some((owner, lim)), false) = (req.owner, new.is_empty()) {
                    let pending: i64 = sqlx::query_scalar(crate::repo::pg_lit("SELECT count(*) FROM jobs WHERE owner_id = $1 AND state IN ('queued', 'running')"))
                        .bind(owner)
                        .fetch_one(&mut *c)
                        .await?;
                    let (jobs, units, bytes): (i64, i64, i64) = sqlx::query_as(crate::repo::pg_lit(
                        "SELECT count(*), COALESCE(sum(units), 0)::bigint, COALESCE(sum(bytes), 0)::bigint FROM jobs WHERE owner_id = $1 AND created_at > $2",
                    ))
                    .bind(owner)
                    .bind(now.saturating_sub(lim.window_secs))
                    .fetch_one(&mut *c)
                    .await?;
                    let add_units: i64 = new.iter().map(|j| i64::from(j.units)).sum();
                    let add_bytes: i64 = new.iter().map(|j| i64::try_from(j.bytes).unwrap_or(i64::MAX)).fold(0i64, i64::saturating_add);
                    let n = i64::try_from(new.len()).unwrap_or(i64::MAX);
                    if pending + n > i64::from(lim.max_pending)
                        || jobs + n > i64::from(lim.max_jobs)
                        || units.saturating_add(add_units) > i64::try_from(lim.max_units).unwrap_or(i64::MAX)
                        || bytes.saturating_add(add_bytes) > i64::try_from(lim.max_bytes).unwrap_or(i64::MAX)
                    {
                        return Err(Error::Quota("submission limit reached".into()).into());
                    }
                }
                let owner = req.owner.map(|(o, _)| o);
                let mut out: Vec<i64> = Vec::with_capacity(known.len());
                for (j, k) in req.jobs.iter().zip(known) {
                    if let Some(id) = k {
                        out.push(id);
                        continue;
                    }
                    let id: i64 = sqlx::query_scalar(crate::repo::pg_lit(
                        "INSERT INTO jobs (kind, domain_id, state, payload, attempts, max_attempts, run_after, epoch, created_at, updated_at, \
                                           dedupe_key, owner_id, units, bytes) \
                         VALUES ($1, $2, 'queued', $3, 0, $4, $5, 0, $5, $5, $6, $7, $8, $9) RETURNING id",
                    ))
                    .bind(j.kind)
                    .bind(j.domain.map(|d: DomainId| d.0))
                    .bind(j.payload)
                    .bind(i64::from(j.max_attempts))
                    .bind(now)
                    .bind(j.dedupe_key)
                    .bind(owner)
                    .bind(i64::from(j.units))
                    .bind(i64::try_from(j.bytes).unwrap_or(i64::MAX))
                    .fetch_one(&mut *c)
                    .await?;
                    out.push(id);
                }
                Ok(out)
            }
        })?;
        Ok(ids.into_iter().map(JobId).collect())
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
                    let Candidate { id, state, attempts, max_attempts, scope, kind, now } = cand;
                    if attempts >= max_attempts {
                        sqlx::query(crate::repo::pg_lit(sql::SETTLE_DEAD))
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
                    // This tenant has had its turn: the others go before it next time.
                    sqlx::query(sql::TURN).bind(scope).bind(&kind).execute(&mut *c).await?;
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
                                created_at: row.try_get("created_at")?,
                            }));
                        }
                        None => {
                            // A row that can never be handed to a worker: park it, keep going.
                            sqlx::query(crate::repo::pg_lit(sql::SETTLE_DEAD))
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
                Ok(sqlx::query(r(sql::HEARTBEAT))
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
                Ok(sqlx::query(r(sql::COMPLETE))
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
                Ok(sqlx::query(r(sql::FAIL))
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

    async fn reschedule(
        &self,
        id: JobId,
        epoch: i64,
        payload: &str,
        error: &str,
        retry_secs: i64,
        now: Option<i64>,
    ) -> Result<()> {
        let n = tx_retry!(self, None, |c| {
            {
                Ok(sqlx::query(r(sql::RESCHEDULE))
                    .bind(id.0)
                    .bind(epoch)
                    .bind(payload)
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
        let ok: Option<i64> = sqlx::query_scalar(r(sql::FENCE))
            .bind(fence.id.0)
            .bind(fence.epoch)
            .bind(now)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?;
        ok.map(|_| ()).ok_or_else(crate::jobs::stale)
    }

    async fn count_by_state(&self, kind: &str, domain: i64) -> Result<Vec<(String, i64)>> {
        use sqlx::Row;
        let rows = sqlx::query(r(sql::COUNT_BY_STATE)).bind(kind).bind(domain).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(|x| Ok((x.try_get::<String, _>(0)?, x.try_get::<i64, _>(1)?))).collect::<std::result::Result<Vec<_>, sqlx::Error>>().map_err(map_db_error)
    }

    async fn queue_figures(&self, kind: &str) -> Result<(Vec<(String, i64)>, i64)> {
        use sqlx::Row;
        let rows = sqlx::query(r(sql::COUNT_ALL_BY_STATE)).bind(kind).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        let counts = rows.iter().map(|x| Ok((x.try_get::<String, _>(0)?, x.try_get::<i64, _>(1)?))).collect::<std::result::Result<Vec<_>, sqlx::Error>>().map_err(map_db_error)?;
        let oldest: i64 = sqlx::query_scalar(r(sql::OLDEST_DUE)).bind(kind).fetch_one(self.store.pool()).await.map_err(map_db_error)?;
        Ok((counts, oldest))
    }

    async fn purge_finished_batch(&self, older_than: i64, limit: i64) -> Result<u64> {
        let n = tx_retry!(self, None, |c| {
            {
                Ok(sqlx::query(
                    crate::repo::pg_lit("DELETE FROM jobs WHERE id IN (\
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
