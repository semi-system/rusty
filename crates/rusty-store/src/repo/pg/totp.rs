//! `TotpRepo` on Postgres.

use async_trait::async_trait;
use rusty_core::{AccountId, Error, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::render_pg as r;
use crate::repo::totp::{sql, TotpRepo, TotpRow};

#[async_trait]
impl TotpRepo for PgRepo {
    async fn get(&self, account: AccountId) -> Result<Option<TotpRow>> {
        let x = sqlx::query(r(sql::GET)).bind(account.0).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.map(|x| Ok(TotpRow { sealed: x.try_get(0)?, confirmed: x.try_get(1)?, last_step: x.try_get(2)? })).transpose().map_err(map_db_error)
    }

    async fn begin(&self, account: AccountId, sealed: &[u8]) -> Result<()> {
        let done = sqlx::query(r(sql::BEGIN)).bind(account.0).bind(sealed).execute(self.store.pool()).await.map_err(map_db_error)?;
        if done.rows_affected() == 0 {
            return Err(Error::Conflict("the second factor is already on".into()));
        }
        Ok(())
    }

    async fn confirm(&self, account: AccountId, step: i64, recovery_hashes: &[String]) -> Result<()> {
        let mut tx = self.store.pool().begin().await.map_err(map_db_error)?;
        let done = sqlx::query(r(sql::CONFIRM)).bind(account.0).bind(step).execute(&mut *tx).await.map_err(map_db_error)?;
        if done.rows_affected() == 0 {
            return Err(Error::Conflict("nothing to confirm".into()));
        }
        sqlx::query(r(sql::CLEAR_RECOVERY)).bind(account.0).execute(&mut *tx).await.map_err(map_db_error)?;
        for h in recovery_hashes {
            sqlx::query(r(sql::ADD_RECOVERY)).bind(account.0).bind(h).execute(&mut *tx).await.map_err(map_db_error)?;
        }
        tx.commit().await.map_err(map_db_error)
    }

    async fn advance(&self, account: AccountId, step: i64) -> Result<bool> {
        Ok(sqlx::query(r(sql::ADVANCE)).bind(account.0).bind(step).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected() > 0)
    }

    async fn recovery(&self, account: AccountId) -> Result<Vec<(i64, String)>> {
        let rows = sqlx::query(r(sql::RECOVERY)).bind(account.0).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(|x| Ok((x.try_get(0)?, x.try_get(1)?))).collect::<std::result::Result<Vec<_>, sqlx::Error>>().map_err(map_db_error)
    }

    async fn spend_recovery(&self, account: AccountId, id: i64) -> Result<bool> {
        Ok(sqlx::query(r(sql::SPEND)).bind(account.0).bind(id).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected() > 0)
    }

    async fn remove(&self, account: AccountId) -> Result<()> {
        sqlx::query(r(sql::REMOVE)).bind(account.0).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(())
    }

    async fn enabled(&self, account: AccountId) -> Result<bool> {
        sqlx::query_scalar(r(sql::ENABLED)).bind(account.0).fetch_one(self.store.pool()).await.map_err(map_db_error)
    }
}
