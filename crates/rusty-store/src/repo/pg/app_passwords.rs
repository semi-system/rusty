//! `AppPasswordRepo` on Postgres: single statements.

use async_trait::async_trait;
use rusty_core::{AccountId, Error, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::app_passwords::{sql, AppPassword, AppPasswordRepo, MAX_APP_PASSWORDS};
use crate::repo::render_pg as r;

#[async_trait]
impl AppPasswordRepo for PgRepo {
    async fn create(&self, account: AccountId, label: &str, hash: &str) -> Result<i64> {
        let have: i64 = sqlx::query_scalar(r(sql::COUNT)).bind(account.0).fetch_one(self.store.pool()).await.map_err(map_db_error)?;
        if have >= MAX_APP_PASSWORDS {
            return Err(Error::Quota("app passwords".into()));
        }
        sqlx::query_scalar(r(sql::CREATE)).bind(account.0).bind(label).bind(hash).fetch_one(self.store.pool()).await.map_err(map_db_error)
    }

    async fn list(&self, account: AccountId) -> Result<Vec<AppPassword>> {
        let rows = sqlx::query(r(sql::LIST)).bind(account.0).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| Ok(AppPassword { id: x.try_get(0)?, label: x.try_get(1)?, created_at: x.try_get(2)?, last_used_at: x.try_get(3)? }))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn hashes(&self, account: AccountId) -> Result<Vec<(i64, String)>> {
        let rows = sqlx::query(r(sql::HASHES)).bind(account.0).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(|x| Ok((x.try_get(0)?, x.try_get(1)?))).collect::<std::result::Result<Vec<_>, sqlx::Error>>().map_err(map_db_error)
    }

    async fn touch(&self, id: i64) -> Result<()> {
        sqlx::query(r(sql::TOUCH)).bind(id).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(())
    }

    async fn revoke(&self, account: AccountId, id: i64) -> Result<bool> {
        Ok(sqlx::query(r(sql::REVOKE)).bind(account.0).bind(id).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected() > 0)
    }
}
