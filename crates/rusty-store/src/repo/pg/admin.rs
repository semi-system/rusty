//! `AdminRepo` on Postgres: single statements, atomic by themselves.

use async_trait::async_trait;
use rusty_core::{AccountId, DomainId, Error, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::admin::{sql, AdminEntry, AdminGrant, AdminRepo, Revoked};
use crate::repo::render_pg as r;

fn grant(x: &sqlx::postgres::PgRow) -> std::result::Result<AdminGrant, sqlx::Error> {
    let domain: Option<i64> = x.try_get(1)?;
    Ok(AdminGrant { account: AccountId(x.try_get(0)?), domain: domain.map(DomainId), created_at: x.try_get(2)? })
}

#[async_trait]
impl AdminRepo for PgRepo {
    async fn put(&self, account: AccountId, domain: Option<DomainId>, now: Option<i64>) -> Result<AdminGrant> {
        let row = sqlx::query(r(sql::PUT))
            .bind(account.0)
            .bind(domain.map(|d| d.0))
            .bind(now)
            .fetch_one(self.store.pool())
            .await
            .map_err(|e| match &e {
                // A missing account or domain is a foreign-key failure.
                sqlx::Error::Database(d) if d.code().as_deref() == Some("23503") => Error::NotFound("account or domain".into()),
                _ => map_db_error(e),
            })?;
        grant(&row).map_err(map_db_error)
    }

    async fn get(&self, account: AccountId) -> Result<Option<AdminGrant>> {
        let x = sqlx::query(r(sql::GET)).bind(account.0).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.as_ref().map(grant).transpose().map_err(map_db_error)
    }

    async fn delete(&self, account: AccountId) -> Result<bool> {
        let done = sqlx::query(r(sql::DELETE)).bind(account.0).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(done.rows_affected() > 0)
    }

    async fn list(&self, after: Option<AccountId>, limit: i64) -> Result<Vec<AdminGrant>> {
        let rows = sqlx::query(r(sql::LIST)).bind(after.map_or(0, |a| a.0)).bind(limit).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(grant).collect::<std::result::Result<Vec<_>, _>>().map_err(map_db_error)
    }

    async fn list_named(&self, after: Option<AccountId>, limit: i64, domain: Option<DomainId>) -> Result<Vec<AdminEntry>> {
        let rows = sqlx::query(r(sql::LIST_NAMED))
            .bind(after.map_or(0, |a| a.0))
            .bind(limit)
            .bind(domain.map(|d| d.0))
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)?;
        rows.iter()
            .map(|x| Ok(AdminEntry { grant: grant(x)?, address: x.try_get(3)? }))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn revoke_keeping_one(&self, account: AccountId) -> Result<Revoked> {
        let mut tx = self.store.pool().begin().await.map_err(map_db_error)?;
        // One lock for every revocation: the count and the delete below are then one step, whoever else is revoking.
        sqlx::query("SELECT pg_advisory_xact_lock(7210001)").execute(&mut *tx).await.map_err(map_db_error)?;
        let found = sqlx::query(r(sql::GET)).bind(account.0).fetch_optional(&mut *tx).await.map_err(map_db_error)?;
        let Some(row) = found else { return Ok(Revoked::NotAdmin) };
        let g = grant(&row).map_err(map_db_error)?;
        if g.domain.is_none() {
            let n: i64 = sqlx::query(r(sql::COUNT_PLATFORM)).fetch_one(&mut *tx).await.map_err(map_db_error)?.try_get(0).map_err(map_db_error)?;
            if n <= 1 {
                return Ok(Revoked::LastPlatform);
            }
        }
        sqlx::query(r(sql::DELETE)).bind(account.0).execute(&mut *tx).await.map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Revoked::Removed)
    }

    async fn count_platform(&self) -> Result<i64> {
        let row = sqlx::query(r(sql::COUNT_PLATFORM)).fetch_one(self.store.pool()).await.map_err(map_db_error)?;
        row.try_get(0).map_err(map_db_error)
    }
}
