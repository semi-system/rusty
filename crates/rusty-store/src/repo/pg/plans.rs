//! `PlanRepo` on Postgres: single statements, atomic by themselves.

use async_trait::async_trait;
use rusty_core::{AccountId, Error, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::plans::{sql, AccountUsage, PlanRepo, PlanRow};
use crate::repo::render_pg as r;
use crate::repo::GLOBAL_PLAN;

#[async_trait]
impl PlanRepo for PgRepo {
    async fn list(&self) -> Result<Vec<PlanRow>> {
        let rows = sqlx::query(r(sql::LIST)).bind(GLOBAL_PLAN).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| Ok(PlanRow { id: x.try_get(0)?, name: x.try_get(1)?, config: x.try_get(2)? }))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn set_account_plan(&self, account: AccountId, plan: Option<&str>) -> Result<()> {
        let done = sqlx::query(r(sql::SET_ACCOUNT_PLAN)).bind(account.0).bind(plan).bind(GLOBAL_PLAN).execute(self.store.pool()).await.map_err(map_db_error)?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound("account or plan".into()));
        }
        Ok(())
    }

    async fn mailbox_used(&self, mailbox: i64) -> Result<i64> {
        let v: Option<i64> = sqlx::query_scalar(r(sql::MAILBOX_USED)).bind(mailbox).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        Ok(v.unwrap_or(0))
    }

    async fn usage_of(&self, accounts: &[i64]) -> Result<Vec<AccountUsage>> {
        if accounts.len() > 500 {
            return Err(Error::Invalid("too many accounts".into()));
        }
        let rows = sqlx::query(r(sql::USAGE_OF)).bind(accounts).bind(GLOBAL_PLAN).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| Ok(AccountUsage { account: x.try_get(0)?, used_bytes: x.try_get(1)?, plan: x.try_get(2)?, last_delivery_at: x.try_get(3)?, quota_bytes: x.try_get(4)? }))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }
}
