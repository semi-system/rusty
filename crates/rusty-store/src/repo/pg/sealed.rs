//! `SealedRepo` on Postgres: single-statement operations, atomic by themselves (an upsert replaces the row whole).

use async_trait::async_trait;
use rusty_core::Result;
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::render_pg as r;
use crate::repo::sealed::{sql, SealedMeta, SealedRepo, SealedRow};

fn meta(x: &sqlx::postgres::PgRow) -> std::result::Result<SealedMeta, sqlx::Error> {
    Ok(SealedMeta {
        owner_kind: x.try_get(0)?,
        owner_id: x.try_get(1)?,
        purpose: x.try_get(2)?,
        key_id: x.try_get(3)?,
        size: x.try_get(4)?,
        created_at: x.try_get(5)?,
        updated_at: x.try_get(6)?,
    })
}

#[async_trait]
impl SealedRepo for PgRepo {
    async fn put(&self, kind: &str, id: i64, purpose: &str, key_id: &str, sealed: &[u8], now: Option<i64>) -> Result<SealedMeta> {
        let row = sqlx::query(r(sql::PUT))
            .bind(kind)
            .bind(id)
            .bind(purpose)
            .bind(key_id)
            .bind(sealed)
            .bind(now)
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)?;
        meta(&row).map_err(map_db_error)
    }

    async fn get(&self, kind: &str, id: i64, purpose: &str) -> Result<Option<SealedRow>> {
        let row = sqlx::query(r(sql::GET)).bind(kind).bind(id).bind(purpose).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        row.map(|x| -> std::result::Result<SealedRow, sqlx::Error> {
            Ok(SealedRow {
                owner_kind: x.try_get(0)?,
                owner_id: x.try_get(1)?,
                purpose: x.try_get(2)?,
                key_id: x.try_get(3)?,
                sealed: x.try_get(4)?,
                created_at: x.try_get(5)?,
                updated_at: x.try_get(6)?,
            })
        })
        .transpose()
        .map_err(map_db_error)
    }

    async fn meta(&self, kind: &str, id: i64) -> Result<Vec<SealedMeta>> {
        let rows = sqlx::query(r(sql::META)).bind(kind).bind(id).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(meta).collect::<std::result::Result<Vec<_>, _>>().map_err(map_db_error)
    }

    async fn delete(&self, kind: &str, id: i64, purpose: &str) -> Result<bool> {
        let done = sqlx::query(r(sql::DELETE)).bind(kind).bind(id).bind(purpose).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(done.rows_affected() > 0)
    }
}
