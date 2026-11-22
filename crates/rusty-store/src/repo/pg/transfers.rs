//! `TransferRepo` on Postgres: single statements.

use async_trait::async_trait;
use rusty_core::{Error, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::render_pg as r;
use crate::repo::transfers::{sql, Transfer, TransferRepo};

fn transfer(x: &sqlx::postgres::PgRow) -> std::result::Result<Transfer, sqlx::Error> {
    Ok(Transfer {
        id: x.try_get(0)?,
        kind: x.try_get(1)?,
        account: x.try_get(2)?,
        requested_by: x.try_get(3)?,
        state: x.try_get(4)?,
        folder: x.try_get(5)?,
        size: x.try_get(6)?,
        messages: x.try_get(7)?,
        skipped: x.try_get(8)?,
        error: x.try_get(9)?,
        created_at: x.try_get(10)?,
        finished_at: x.try_get(11)?,
        expires_at: x.try_get(12)?,
    })
}

#[async_trait]
impl TransferRepo for PgRepo {
    async fn create(&self, kind: &str, account: i64, requested_by: &str, folder: Option<&str>) -> Result<i64> {
        let by: String = requested_by.chars().take(320).collect();
        sqlx::query_scalar(r(sql::CREATE)).bind(kind).bind(account).bind(by).bind(folder).fetch_one(self.store.pool()).await.map_err(map_db_error)
    }

    async fn add_chunk(&self, id: i64, seq: i32, data: &[u8], max_bytes: i64) -> Result<bool> {
        let len = i64::try_from(data.len()).map_err(|_| Error::Invalid("chunk".into()))?;
        let n = sqlx::query(r(sql::ADD_CHUNK))
            .bind(id)
            .bind(seq)
            .bind(data)
            .bind(len)
            .bind(max_bytes)
            .execute(self.store.pool())
            .await
            .map_err(|e| match &e {
                // The same piece twice.
                sqlx::Error::Database(d) if d.code().as_deref() == Some("23505") => Error::Conflict("chunk".into()),
                _ => map_db_error(e),
            })?
            .rows_affected();
        Ok(n > 0)
    }

    async fn chunk(&self, id: i64, seq: i32) -> Result<Option<Vec<u8>>> {
        sqlx::query_scalar(r(sql::CHUNK)).bind(id).bind(seq).fetch_optional(self.store.pool()).await.map_err(map_db_error)
    }

    async fn queue_import(&self, id: i64) -> Result<bool> {
        Ok(sqlx::query(r(sql::QUEUE_IMPORT)).bind(id).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected() > 0)
    }

    async fn claim(&self) -> Result<Option<Transfer>> {
        let x = sqlx::query(r(sql::CLAIM)).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.as_ref().map(transfer).transpose().map_err(map_db_error)
    }

    async fn finish(&self, id: i64, state: &str, size: i64, messages: i64, skipped: i64, error: Option<&str>) -> Result<()> {
        sqlx::query(r(sql::FINISH)).bind(id).bind(state).bind(size).bind(messages).bind(skipped).bind(error).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(())
    }

    async fn get(&self, id: i64) -> Result<Option<Transfer>> {
        let x = sqlx::query(r(sql::GET)).bind(id).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.as_ref().map(transfer).transpose().map_err(map_db_error)
    }

    async fn list_for_account(&self, account: i64, limit: i64) -> Result<Vec<Transfer>> {
        let rows = sqlx::query(r(sql::LIST)).bind(account).bind(limit).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(transfer).collect::<std::result::Result<Vec<_>, _>>().map_err(map_db_error)
    }

    async fn purge_expired(&self, limit: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::PURGE)).bind(limit).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }

    async fn fail_stuck(&self, older_than_secs: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::FAIL_STUCK)).bind(older_than_secs).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }
}
