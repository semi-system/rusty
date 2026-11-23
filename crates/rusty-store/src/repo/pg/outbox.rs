//! `OutboxRepo` on Postgres: single statements.

use async_trait::async_trait;
use rusty_core::{AccountId, Error, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::outbox::{sql, OutboxItem, OutboxRepo, MAX_QUEUED_PER_ACCOUNT};
use crate::repo::render_pg as r;

fn item(x: &sqlx::postgres::PgRow) -> std::result::Result<OutboxItem, sqlx::Error> {
    let recipients: String = x.try_get(5)?;
    Ok(OutboxItem {
        id: x.try_get(0)?,
        account: AccountId(x.try_get(1)?),
        sender: x.try_get(2)?,
        blob_hash: x.try_get(3)?,
        size: x.try_get(4)?,
        recipients: serde_json::from_str(&recipients).unwrap_or_default(),
        subject: x.try_get(6)?,
        state: x.try_get(7)?,
        error: x.try_get(8)?,
        created_at: x.try_get(9)?,
        finished_at: x.try_get(10)?,
    })
}

#[async_trait]
impl OutboxRepo for PgRepo {
    async fn enqueue(&self, account: AccountId, sender: &str, blob_hash: &str, size: i64, recipients: &[String], subject: &str) -> Result<i64> {
        let waiting: i64 = sqlx::query_scalar(r(sql::COUNT_QUEUED)).bind(account.0).fetch_one(self.store.pool()).await.map_err(map_db_error)?;
        if waiting >= MAX_QUEUED_PER_ACCOUNT {
            return Err(Error::Quota("outbox".into()));
        }
        let json = serde_json::to_string(recipients).map_err(|_| Error::Invalid("recipients".into()))?;
        let subject: String = subject.chars().take(200).collect();
        sqlx::query_scalar(r(sql::ENQUEUE)).bind(account.0).bind(sender).bind(blob_hash).bind(size).bind(json).bind(subject).fetch_one(self.store.pool()).await.map_err(map_db_error)
    }

    async fn list(&self, account: AccountId, limit: i64) -> Result<Vec<OutboxItem>> {
        let rows = sqlx::query(r(sql::LIST)).bind(account.0).bind(limit).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(item).collect::<std::result::Result<Vec<_>, _>>().map_err(map_db_error)
    }

    async fn discard(&self, account: AccountId, id: i64) -> Result<bool> {
        Ok(sqlx::query(r(sql::DISCARD)).bind(account.0).bind(id).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected() > 0)
    }

    async fn claim(&self) -> Result<Option<OutboxItem>> {
        let x = sqlx::query(r(sql::CLAIM)).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.as_ref().map(item).transpose().map_err(map_db_error)
    }

    async fn finish(&self, id: i64, state: &str, error: Option<&str>) -> Result<()> {
        sqlx::query(r(sql::FINISH)).bind(id).bind(state).bind(error).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(())
    }

    async fn fail_stuck(&self, older_than_secs: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::FAIL_STUCK)).bind(older_than_secs).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }

    async fn purge_finished(&self, older_than_secs: i64, limit: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::PURGE)).bind(older_than_secs).bind(limit).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }
}
