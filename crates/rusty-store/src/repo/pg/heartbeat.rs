//! `HeartbeatRepo` on Postgres: single statements, atomic by themselves.

use async_trait::async_trait;
use rusty_core::Result;
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::heartbeat::{sql, HeartbeatRepo, HeartbeatRow};

#[async_trait]
impl HeartbeatRepo for PgRepo {
    async fn beat(&self, service: &str, instance: &str, version: &str, uptime_secs: i64, status: &str, detail: &str) -> Result<()> {
        sqlx::query(sql::BEAT)
            .bind(service)
            .bind(instance)
            .bind(version)
            .bind(uptime_secs)
            .bind(status)
            .bind(detail)
            .execute(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<HeartbeatRow>> {
        let rows = sqlx::query(sql::LIST).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| {
                Ok(HeartbeatRow {
                    service: x.try_get(0)?,
                    instance: x.try_get(1)?,
                    version: x.try_get(2)?,
                    uptime_secs: x.try_get(3)?,
                    age_secs: x.try_get(4)?,
                    status: x.try_get(5)?,
                    detail: x.try_get(6)?,
                })
            })
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn prune(&self, older_than_secs: i64) -> Result<u64> {
        let done = sqlx::query(sql::PRUNE).bind(older_than_secs).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(done.rows_affected())
    }
}
