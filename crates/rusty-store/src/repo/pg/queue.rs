//! `QueueRepo` on Postgres: single statements, atomic by themselves.

use async_trait::async_trait;
use rusty_core::{DomainId, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::queue::{sql, StatRow, InboundRow, LogRow, NewLogRow, QueueJob, QueueRepo, HELD_UNTIL};
use crate::repo::render_pg as r;

#[async_trait]
impl QueueRepo for PgRepo {
    async fn jobs(&self, domain: DomainId, state: Option<&str>, needle: Option<&str>, before: Option<i64>, limit: i64) -> Result<Vec<QueueJob>> {
        let rows = sqlx::query(r(sql::JOBS))
            .bind(domain.0)
            .bind(state)
            .bind(needle)
            .bind(before)
            .bind(limit)
            .bind(HELD_UNTIL)
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)?;
        rows.iter()
            .map(|x| {
                Ok(QueueJob { id: x.try_get(0)?, state: x.try_get(1)?, attempts: x.try_get(2)?, created_at: x.try_get(3)?, run_after: x.try_get(4)?, last_error: x.try_get(5)?, payload: x.try_get(6)? })
            })
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn hold(&self, domain: DomainId, job: i64) -> Result<bool> {
        let n = sqlx::query(r(sql::HOLD)).bind(job).bind(domain.0).bind(HELD_UNTIL).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected();
        Ok(n == 1)
    }

    async fn release(&self, domain: DomainId, job: i64) -> Result<bool> {
        let n = sqlx::query(r(sql::RELEASE)).bind(job).bind(domain.0).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected();
        Ok(n == 1)
    }

    async fn requeue(&self, domain: DomainId, job: i64) -> Result<bool> {
        let n = sqlx::query(r(sql::REQUEUE)).bind(job).bind(domain.0).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected();
        Ok(n == 1)
    }

    async fn delete(&self, domain: DomainId, job: i64) -> Result<bool> {
        let n = sqlx::query(r(sql::DELETE)).bind(job).bind(domain.0).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected();
        Ok(n == 1)
    }

    async fn trace_held(&self, domain: DomainId, needle: Option<&str>, before: Option<i64>, limit: i64, held_only: bool) -> Result<Vec<LogRow>> {
        let rows = sqlx::query(r(sql::TRACE)).bind(domain.0).bind(needle).bind(before).bind(limit).bind(held_only).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| {
                Ok(LogRow {
                    id: x.try_get(0)?,
                    direction: x.try_get(1)?,
                    at: x.try_get(2)?,
                    job_id: x.try_get(3)?,
                    sender: x.try_get(4)?,
                    recipient: x.try_get(5)?,
                    event: x.try_get(6)?,
                    remote: x.try_get(7)?,
                    status: x.try_get(8)?,
                    detail: x.try_get(9)?,
                })
            })
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn log(&self, rows: &[NewLogRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let cut = |s: &str, n: usize| -> String { s.chars().take(n).collect() };
        let domains: Vec<i64> = rows.iter().map(|x| x.domain.0).collect();
        let jobs: Vec<Option<i64>> = rows.iter().map(|x| x.job_id).collect();
        let senders: Vec<String> = rows.iter().map(|x| cut(&x.sender, 320)).collect();
        let recipients: Vec<String> = rows.iter().map(|x| cut(&x.recipient, 320)).collect();
        let events: Vec<&str> = rows.iter().map(|x| x.event).collect();
        let remotes: Vec<Option<String>> = rows.iter().map(|x| x.remote.as_deref().map(|s| cut(s, 255))).collect();
        let statuses: Vec<Option<String>> = rows.iter().map(|x| x.status.as_deref().map(|s| cut(s, 16))).collect();
        let details: Vec<Option<String>> = rows.iter().map(|x| x.detail.as_deref().map(|s| cut(s, 512))).collect();
        sqlx::query(r(sql::LOG))
            .bind(&domains)
            .bind(&jobs)
            .bind(&senders)
            .bind(&recipients)
            .bind(&events)
            .bind(&remotes)
            .bind(&statuses)
            .bind(&details)
            .execute(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    async fn log_inbound(&self, rows: &[InboundRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let cut = |s: &str, n: usize| -> String { s.chars().take(n).collect() };
        let senders: Vec<String> = rows.iter().map(|x| cut(&x.sender, 320)).collect();
        let recipients: Vec<String> = rows.iter().map(|x| cut(&x.recipient, 320)).collect();
        let events: Vec<&str> = rows.iter().map(|x| x.event).collect();
        let remotes: Vec<String> = rows.iter().map(|x| cut(&x.remote, 255)).collect();
        let statuses: Vec<Option<String>> = rows.iter().map(|x| x.status.as_deref().map(|s| cut(s, 16))).collect();
        let details: Vec<Option<String>> = rows.iter().map(|x| x.detail.as_deref().map(|s| cut(s, 512))).collect();
        sqlx::query(r(sql::LOG_INBOUND))
            .bind(&senders)
            .bind(&recipients)
            .bind(&events)
            .bind(&remotes)
            .bind(&statuses)
            .bind(&details)
            .execute(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    async fn roll_up_stats(&self, now: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::ROLL_UP)).bind(now.div_euclid(86_400)).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }

    async fn purge_stats(&self, older_than_day: i64, limit: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::PURGE_STATS)).bind(older_than_day).bind(limit).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }

    async fn stats(&self, domain: DomainId, since: i64, now: i64) -> Result<Vec<StatRow>> {
        // The log is trusted for the days of the last 28 (the roll-up window); the days before come from the roll-up.
        let trusted_from = now.div_euclid(86_400) - 28;
        let rows = sqlx::query(r(sql::STATS)).bind(domain.0).bind(since).bind(trusted_from).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| Ok(StatRow { day: x.try_get(0)?, direction: x.try_get(1)?, event: x.try_get(2)?, count: x.try_get(3)? }))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn purge_log(&self, older_than: i64, limit: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::PURGE_LOG)).bind(older_than).bind(limit).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }
}
