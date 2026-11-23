//! `BackupRepo` on Postgres: single statements.

use async_trait::async_trait;
use rusty_core::Result;
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::backups::{sql, BackupRepo, BackupRun};
use crate::repo::render_pg as r;

#[async_trait]
impl BackupRepo for PgRepo {
    async fn list(&self, limit: i64) -> Result<Vec<BackupRun>> {
        let rows = sqlx::query(r(sql::LIST)).bind(limit).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| Ok(BackupRun { id: x.try_get(0)?, at: x.try_get(1)?, kind: x.try_get(2)?, ok: x.try_get(3)?, bytes: x.try_get(4)?, detail: x.try_get(5)? }))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn purge(&self, older_than: i64, limit: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::PURGE)).bind(older_than).bind(limit).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }
}
