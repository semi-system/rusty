//! `RelayRepo` on Postgres: single statements, atomic by themselves.

use async_trait::async_trait;
use rusty_core::{DomainId, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::relay::{sql, RelayRepo, RelayRow};
use crate::repo::render_pg as r;

fn row(x: &sqlx::postgres::PgRow) -> std::result::Result<RelayRow, sqlx::Error> {
    let port: i32 = x.try_get(2)?;
    Ok(RelayRow {
        domain: DomainId(x.try_get(0)?),
        host: x.try_get(1)?,
        port: u16::try_from(port).map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
        tls: x.try_get(3)?,
        username: x.try_get(4)?,
        spf_include: x.try_get(5)?,
        check_status: x.try_get(6)?,
        checked_at: x.try_get(7)?,
        created_at: x.try_get(8)?,
        updated_at: x.try_get(9)?,
    })
}

#[async_trait]
impl RelayRepo for PgRepo {
    async fn put(&self, v: &RelayRow, now: Option<i64>) -> Result<RelayRow> {
        let done = sqlx::query(r(sql::PUT))
            .bind(v.domain.0)
            .bind(&v.host)
            .bind(i32::from(v.port))
            .bind(&v.tls)
            .bind(&v.username)
            .bind(&v.spf_include)
            .bind(now)
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)?;
        row(&done).map_err(map_db_error)
    }

    async fn get(&self, domain: DomainId) -> Result<Option<RelayRow>> {
        let x = sqlx::query(r(sql::GET)).bind(domain.0).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.as_ref().map(row).transpose().map_err(map_db_error)
    }

    async fn delete(&self, domain: DomainId) -> Result<bool> {
        let done = sqlx::query(r(sql::DELETE)).bind(domain.0).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(done.rows_affected() > 0)
    }

    async fn record_check(&self, domain: DomainId, status: &str, now: Option<i64>) -> Result<bool> {
        let done = sqlx::query(r(sql::RECORD)).bind(domain.0).bind(status).bind(now).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(done.rows_affected() > 0)
    }
}
