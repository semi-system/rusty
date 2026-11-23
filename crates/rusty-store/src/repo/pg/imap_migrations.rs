//! `MigrationRepo` on Postgres: single statements.

use async_trait::async_trait;
use rusty_core::Result;
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::imap_migrations::{sql, Migration, MigrationRepo, NewMigration, Progress};
use crate::repo::render_pg as r;

fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn migration(x: &sqlx::postgres::PgRow) -> std::result::Result<Migration, sqlx::Error> {
    Ok(Migration {
        id: x.try_get(0)?,
        account: x.try_get(1)?,
        requested_by: x.try_get(2)?,
        host: x.try_get(3)?,
        port: x.try_get(4)?,
        security: x.try_get(5)?,
        remote_user: x.try_get(6)?,
        state: x.try_get(7)?,
        cancel_requested: x.try_get(8)?,
        folder: x.try_get(9)?,
        folders_total: x.try_get(10)?,
        folders_done: x.try_get(11)?,
        messages: x.try_get(12)?,
        skipped: x.try_get(13)?,
        bytes: x.try_get(14)?,
        error: x.try_get(15)?,
        created_at: x.try_get(16)?,
        updated_at: x.try_get(17)?,
        finished_at: x.try_get(18)?,
    })
}

#[async_trait]
impl MigrationRepo for PgRepo {
    async fn create(&self, m: &NewMigration) -> Result<i64> {
        sqlx::query_scalar(r(sql::CREATE))
            .bind(m.account)
            .bind(cut(&m.requested_by, 320))
            .bind(cut(&m.host, 253))
            .bind(m.port)
            .bind(&m.security)
            .bind(cut(&m.remote_user, 320))
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)
    }

    async fn queue(&self, id: i64) -> Result<bool> {
        Ok(sqlx::query(r(sql::QUEUE)).bind(id).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected() > 0)
    }

    async fn get(&self, id: i64) -> Result<Option<Migration>> {
        let x = sqlx::query(r(sql::GET)).bind(id).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.as_ref().map(migration).transpose().map_err(map_db_error)
    }

    async fn list_for_account(&self, account: i64, limit: i64) -> Result<Vec<Migration>> {
        let rows = sqlx::query(r(sql::LIST)).bind(account).bind(limit).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(migration).collect::<std::result::Result<Vec<_>, _>>().map_err(map_db_error)
    }

    async fn claim(&self) -> Result<Option<Migration>> {
        let x = sqlx::query(r(sql::CLAIM)).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.as_ref().map(migration).transpose().map_err(map_db_error)
    }

    async fn progress(&self, id: i64, p: &Progress) -> Result<bool> {
        let cancel: Option<bool> = sqlx::query_scalar(r(sql::PROGRESS))
            .bind(id)
            .bind(p.folder.as_deref().map(|f| cut(f, 200)))
            .bind(p.folders_total)
            .bind(p.folders_done)
            .bind(p.messages)
            .bind(p.skipped)
            .bind(p.bytes)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?;
        // `None`: it is not `running` any more (stopped by the sweep); `Some(true)`: the administrator asked to stop.
        Ok(cancel == Some(false))
    }

    async fn finish(&self, id: i64, state: &str, error: Option<&str>, p: &Progress) -> Result<()> {
        sqlx::query(r(sql::FINISH))
            .bind(id)
            .bind(state)
            .bind(error)
            .bind(p.folder.as_deref().map(|f| cut(f, 200)))
            .bind(p.folders_total)
            .bind(p.folders_done)
            .bind(p.messages)
            .bind(p.skipped)
            .bind(p.bytes)
            .execute(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    async fn request_cancel(&self, id: i64) -> Result<bool> {
        let queued = sqlx::query(r(sql::CANCEL_QUEUED)).bind(id).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected();
        if queued > 0 {
            return Ok(true);
        }
        Ok(sqlx::query(r(sql::CANCEL_RUNNING)).bind(id).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected() > 0)
    }

    async fn purge_expired(&self, limit: i64) -> Result<u64> {
        let n: i64 = sqlx::query_scalar(r(sql::PURGE)).bind(limit).fetch_one(self.store.pool()).await.map_err(map_db_error)?;
        // The passwords of what is gone go with it.
        sqlx::query(r(sql::PURGE_SECRETS)).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    async fn fail_stuck(&self, older_than_secs: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::FAIL_STUCK)).bind(older_than_secs).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }
}
