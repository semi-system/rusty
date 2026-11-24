//! `SieveRepo` on Postgres.

use async_trait::async_trait;
use rusty_core::{AccountId, Error, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::render_pg as r;
use crate::repo::sieve::{sql, ScriptInfo, SieveRepo, MAX_SCRIPTS, MAX_SCRIPT_BYTES};

#[async_trait]
impl SieveRepo for PgRepo {
    async fn active_body(&self, account: AccountId) -> Result<Option<String>> {
        sqlx::query_scalar(r(sql::ACTIVE)).bind(account.0).fetch_optional(self.store.pool()).await.map_err(map_db_error)
    }

    async fn list(&self, account: AccountId) -> Result<Vec<ScriptInfo>> {
        let rows = sqlx::query(r(sql::LIST)).bind(account.0).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| Ok(ScriptInfo { name: x.try_get(0)?, active: x.try_get(1)?, size: usize::try_from(x.try_get::<i32, _>(2)?).unwrap_or(0) }))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn get(&self, account: AccountId, name: &str) -> Result<Option<String>> {
        sqlx::query_scalar(r(sql::GET)).bind(account.0).bind(name).fetch_optional(self.store.pool()).await.map_err(map_db_error)
    }

    async fn put(&self, account: AccountId, name: &str, body: &str) -> Result<()> {
        if body.len() > MAX_SCRIPT_BYTES {
            return Err(Error::Invalid("script too large".into()));
        }
        let pool = self.store.pool();
        let exists: bool = sqlx::query_scalar(r(sql::EXISTS)).bind(account.0).bind(name).fetch_one(pool).await.map_err(map_db_error)?;
        if !exists {
            let have: i64 = sqlx::query_scalar(r(sql::COUNT)).bind(account.0).fetch_one(pool).await.map_err(map_db_error)?;
            if have >= MAX_SCRIPTS {
                return Err(Error::Quota("scripts".into()));
            }
        }
        sqlx::query(r(sql::PUT)).bind(account.0).bind(name).bind(body).execute(pool).await.map_err(map_db_error)?;
        Ok(())
    }

    async fn delete(&self, account: AccountId, name: &str) -> Result<bool> {
        let pool = self.store.pool();
        if sqlx::query_scalar::<_, bool>(r(sql::IS_ACTIVE)).bind(account.0).bind(name).fetch_optional(pool).await.map_err(map_db_error)? == Some(true) {
            return Err(Error::Conflict("the script is active".into()));
        }
        Ok(sqlx::query(r(sql::DELETE)).bind(account.0).bind(name).execute(pool).await.map_err(map_db_error)?.rows_affected() > 0)
    }

    async fn set_active(&self, account: AccountId, name: Option<&str>) -> Result<()> {
        let mut tx = self.store.pool().begin().await.map_err(map_db_error)?;
        if let Some(n) = name {
            let there: bool = sqlx::query_scalar(r(sql::EXISTS)).bind(account.0).bind(n).fetch_one(&mut *tx).await.map_err(map_db_error)?;
            if !there {
                return Err(Error::NotFound("script".into()));
            }
        }
        sqlx::query(r(sql::DEACTIVATE)).bind(account.0).execute(&mut *tx).await.map_err(map_db_error)?;
        if let Some(n) = name {
            sqlx::query(r(sql::ACTIVATE)).bind(account.0).bind(n).execute(&mut *tx).await.map_err(map_db_error)?;
        }
        tx.commit().await.map_err(map_db_error)
    }
}
