//! `AuditRepo` on Postgres: single statements.

use async_trait::async_trait;
use rusty_core::Result;
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::audit::{sql, AuditRepo, AuditRow, NewAuditRow};
use crate::repo::render_pg as r;

#[async_trait]
impl AuditRepo for PgRepo {
    async fn record(&self, row: &NewAuditRow) -> Result<()> {
        let cut = |s: &str, n: usize| -> String { s.chars().take(n).collect() };
        sqlx::query(r(sql::RECORD))
            .bind(row.actor_id)
            .bind(cut(&row.actor, 320))
            .bind(row.domain_id)
            .bind(cut(&row.ip, 64))
            .bind(&row.method)
            .bind(cut(&row.route, 200))
            .bind(cut(&row.path, 512))
            .bind(row.status)
            .execute(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    async fn list(&self, domain: Option<i64>, before: Option<i64>, limit: i64) -> Result<Vec<AuditRow>> {
        let rows = sqlx::query(r(sql::LIST)).bind(domain).bind(before).bind(limit).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| {
                Ok(AuditRow { id: x.try_get(0)?, at: x.try_get(1)?, actor: x.try_get(2)?, domain_id: x.try_get(3)?, ip: x.try_get(4)?, method: x.try_get(5)?, route: x.try_get(6)?, path: x.try_get(7)?, status: x.try_get(8)? })
            })
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn purge(&self, older_than: i64, limit: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::PURGE)).bind(older_than).bind(limit).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }
}
