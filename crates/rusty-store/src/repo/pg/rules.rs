//! `RulesRepo` on Postgres: single statements.

use async_trait::async_trait;
use rusty_core::Result;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::render_pg as r;
use crate::repo::rules::{sql, RulesRepo};

#[async_trait]
impl RulesRepo for PgRepo {
    async fn claim_vacation_reply(&self, account: i64, sender: &str, window_secs: i64) -> Result<bool> {
        let sender: String = sender.chars().take(320).collect();
        let n = sqlx::query(r(sql::CLAIM)).bind(account).bind(sender).bind(window_secs).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected();
        Ok(n > 0)
    }

    async fn purge_vacation_replies(&self, older_than: i64, limit: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::PURGE)).bind(older_than).bind(limit).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }
}
