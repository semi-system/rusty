//! `BlobStoreRepo` on Postgres.

use async_trait::async_trait;
use rusty_core::{DomainId, Error, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::blob_store::{sql, BlobStoreRepo, BlobStoreRow, StoreId, PLATFORM_STORE};
use crate::repo::render_pg as r;

fn row(x: &sqlx::postgres::PgRow) -> std::result::Result<BlobStoreRow, sqlx::Error> {
    Ok(BlobStoreRow {
        id: StoreId(x.try_get(0)?),
        domain: x.try_get::<Option<i64>, _>(1)?.map(DomainId),
        kind: x.try_get(2)?,
        config: x.try_get(3)?,
        check_status: x.try_get(4)?,
        checked_at: x.try_get(5)?,
    })
}

#[async_trait]
impl BlobStoreRepo for PgRepo {

    async fn get(&self, id: StoreId) -> Result<Option<BlobStoreRow>> {
        let x = sqlx::query(r(sql::GET)).bind(id.0).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.as_ref().map(row).transpose().map_err(map_db_error)
    }

    async fn for_domain(&self, domain: DomainId) -> Result<Option<BlobStoreRow>> {
        let x = sqlx::query(r(sql::FOR_DOMAIN)).bind(domain.0).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.as_ref().map(row).transpose().map_err(map_db_error)
    }

    async fn update_config(&self, id: StoreId, config: &str) -> Result<()> {
        let mut tx = self.store.pool().begin().await.map_err(map_db_error)?;
        // The store row is locked: a first body cannot be written between the check and the change.
        let found: Option<i64> = sqlx::query_scalar("SELECT id FROM blob_stores WHERE id = $1 AND domain_id IS NOT NULL FOR UPDATE").bind(id.0).fetch_optional(&mut *tx).await.map_err(map_db_error)?;
        if found.is_none() {
            return Err(Error::NotFound("blob store".into()));
        }
        let has: bool = sqlx::query_scalar(r(sql::HAS_BLOBS)).bind(id.0).fetch_one(&mut *tx).await.map_err(map_db_error)?;
        if has {
            return Err(Error::Conflict("the store already holds bodies: its settings cannot change".into()));
        }
        sqlx::query(r(sql::UPDATE_CONFIG)).bind(id.0).bind(config).execute(&mut *tx).await.map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(())
    }

    async fn record_check(&self, id: StoreId, status: &str, now: Option<i64>) -> Result<()> {
        let done = sqlx::query(r(sql::RECORD_CHECK)).bind(id.0).bind(status).bind(now).execute(self.store.pool()).await.map_err(map_db_error)?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound("blob store".into()));
        }
        Ok(())
    }

    async fn list(&self) -> Result<Vec<BlobStoreRow>> {
        let rows = sqlx::query(r(sql::LIST)).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(row).collect::<std::result::Result<Vec<_>, _>>().map_err(map_db_error)
    }

    async fn create_for_domain(&self, domain: DomainId, kind: &str, config: &str) -> Result<StoreId> {
        if !matches!(kind, "fs" | "s3") {
            return Err(Error::Invalid("a domain's own blob store is of kind fs or s3".into()));
        }
        let mut tx = self.store.pool().begin().await.map_err(map_db_error)?;
        // The domain row is locked: two requests cannot both give it a store, and a delivery cannot slip in a first message between the check and the switch.
        let current: Option<i64> = sqlx::query_scalar(r(sql::DOMAIN_LOCK)).bind(domain.0).fetch_optional(&mut *tx).await.map_err(map_db_error)?;
        let Some(current) = current else { return Err(Error::NotFound("domain".into())) };
        if StoreId(current) != PLATFORM_STORE {
            return Err(Error::Conflict("the domain already has a blob store of its own".into()));
        }
        let has: bool = sqlx::query_scalar(r(sql::DOMAIN_HAS_MESSAGES)).bind(domain.0).fetch_one(&mut *tx).await.map_err(map_db_error)?;
        if has {
            return Err(Error::Conflict("the domain already holds messages: bodies are not moved between stores".into()));
        }
        let id: i64 = sqlx::query_scalar(r(sql::INSERT)).bind(domain.0).bind(kind).bind(config).fetch_one(&mut *tx).await.map_err(map_db_error)?;
        sqlx::query(r(sql::POINT_DOMAIN)).bind(domain.0).bind(id).execute(&mut *tx).await.map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(StoreId(id))
    }
}
