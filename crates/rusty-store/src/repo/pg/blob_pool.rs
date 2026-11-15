//! `BlobPoolRepo` on Postgres.

use async_trait::async_trait;
use rusty_core::{BlobHash, Error, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::blob_pool::{sql, BackendId, BackendRow, BackendState, BlobPoolRepo, Location, MAX_BACKENDS};
use crate::repo::render_pg as r;
use crate::repo::{hash_bytes, StoreId};

fn row(x: &sqlx::postgres::PgRow) -> std::result::Result<BackendRow, sqlx::Error> {
    let weight: i32 = x.try_get(5)?;
    let state: String = x.try_get(6)?;
    Ok(BackendRow {
        id: BackendId(x.try_get(0)?),
        store: StoreId(x.try_get(1)?),
        name: x.try_get(2)?,
        kind: x.try_get(3)?,
        config: x.try_get(4)?,
        weight: u32::try_from(weight).unwrap_or(1),
        state: BackendState::parse(&state).unwrap_or(BackendState::Disabled),
        check_status: x.try_get(7)?,
        checked_at: x.try_get(8)?,
    })
}

fn hash_of(bytes: &[u8]) -> Option<BlobHash> {
    BlobHash::parse(&hex::encode(bytes)).ok()
}

fn lim(n: u32) -> i64 {
    i64::from(n.clamp(1, 10_000))
}

#[async_trait]
impl BlobPoolRepo for PgRepo {
    async fn backends(&self, store: StoreId) -> Result<Vec<BackendRow>> {
        let rows = sqlx::query(r(sql::BACKENDS)).bind(store.0).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(row).collect::<std::result::Result<Vec<_>, _>>().map_err(map_db_error)
    }

    async fn backend(&self, id: BackendId) -> Result<Option<BackendRow>> {
        let x = sqlx::query(r(sql::BACKEND)).bind(id.0).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.as_ref().map(row).transpose().map_err(map_db_error)
    }

    async fn add_backend(&self, store: StoreId, name: &str, kind: &str, config: &str, weight: u32) -> Result<BackendId> {
        let mut tx = self.store.pool().begin().await.map_err(map_db_error)?;
        // The store row is locked: two requests cannot both take the last place.
        let store_kind: Option<String> = sqlx::query_scalar(r(sql::STORE_LOCK)).bind(store.0).fetch_optional(&mut *tx).await.map_err(map_db_error)?;
        if store_kind.as_deref() != Some("pool") {
            return Err(Error::NotFound("blob pool".into()));
        }
        let n: i64 = sqlx::query_scalar(r(sql::COUNT_BACKENDS)).bind(store.0).fetch_one(&mut *tx).await.map_err(map_db_error)?;
        if usize::try_from(n).unwrap_or(usize::MAX) >= MAX_BACKENDS {
            return Err(Error::Conflict("a pool has at most 8 backends".into()));
        }
        let weight = i32::try_from(weight).map_err(|_| Error::Invalid("weight".into()))?;
        let id: i64 = sqlx::query_scalar(r(sql::ADD_BACKEND)).bind(store.0).bind(name).bind(kind).bind(config).bind(weight).fetch_one(&mut *tx).await.map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(BackendId(id))
    }

    async fn update_backend_config(&self, id: BackendId, config: &str) -> Result<()> {
        let mut tx = self.store.pool().begin().await.map_err(map_db_error)?;
        let found: Option<i64> = sqlx::query_scalar(r(sql::BACKEND_LOCK)).bind(id.0).fetch_optional(&mut *tx).await.map_err(map_db_error)?;
        if found.is_none() {
            return Err(Error::NotFound("blob backend".into()));
        }
        let held: i64 = sqlx::query_scalar(r(sql::COUNT_HELD)).bind(id.0).fetch_one(&mut *tx).await.map_err(map_db_error)?;
        if held > 0 {
            return Err(Error::Conflict("the backend already holds objects: its settings cannot change".into()));
        }
        sqlx::query(r(sql::UPDATE_BACKEND_CONFIG)).bind(id.0).bind(config).execute(&mut *tx).await.map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(())
    }

    async fn set_backend_state(&self, id: BackendId, state: BackendState) -> Result<()> {
        let done = sqlx::query(r(sql::SET_BACKEND_STATE)).bind(id.0).bind(state.name()).execute(self.store.pool()).await.map_err(map_db_error)?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound("blob backend".into()));
        }
        Ok(())
    }

    async fn set_backend_weight(&self, id: BackendId, weight: u32) -> Result<()> {
        let weight = i32::try_from(weight).map_err(|_| Error::Invalid("weight".into()))?;
        let done = sqlx::query(r(sql::SET_BACKEND_WEIGHT)).bind(id.0).bind(weight).execute(self.store.pool()).await.map_err(map_db_error)?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound("blob backend".into()));
        }
        Ok(())
    }

    async fn record_backend_check(&self, id: BackendId, status: &str, now: Option<i64>) -> Result<()> {
        let done = sqlx::query(r(sql::RECORD_BACKEND_CHECK)).bind(id.0).bind(status).bind(now).execute(self.store.pool()).await.map_err(map_db_error)?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound("blob backend".into()));
        }
        Ok(())
    }

    async fn remove_backend(&self, id: BackendId) -> Result<()> {
        let mut tx = self.store.pool().begin().await.map_err(map_db_error)?;
        let found: Option<i64> = sqlx::query_scalar(r(sql::BACKEND_LOCK)).bind(id.0).fetch_optional(&mut *tx).await.map_err(map_db_error)?;
        if found.is_none() {
            return Err(Error::NotFound("blob backend".into()));
        }
        let held: i64 = sqlx::query_scalar(r(sql::COUNT_HELD)).bind(id.0).fetch_one(&mut *tx).await.map_err(map_db_error)?;
        if held > 0 {
            return Err(Error::Conflict("the backend still holds objects: drain it first".into()));
        }
        sqlx::query(r(sql::REMOVE_BACKEND)).bind(id.0).execute(&mut *tx).await.map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(())
    }

    async fn add_locations(&self, store: StoreId, hash: &BlobHash, backends: &[BackendId]) -> Result<()> {
        if backends.is_empty() {
            return Ok(());
        }
        let ids: Vec<i64> = backends.iter().map(|b| b.0).collect();
        sqlx::query(r(sql::ADD_LOCATIONS)).bind(store.0).bind(hash_bytes(hash)?).bind(ids).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(())
    }

    async fn locations(&self, store: StoreId, hash: &BlobHash) -> Result<Vec<Location>> {
        let rows = sqlx::query(r(sql::LOCATIONS)).bind(store.0).bind(hash_bytes(hash)?).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| Ok(Location { backend: BackendId(x.try_get(0)?), pending_delete: x.try_get::<String, _>(1)? == "pending_delete" }))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn mark_all_pending_delete(&self, store: StoreId, hash: &BlobHash) -> Result<()> {
        sqlx::query(r(sql::MARK_PENDING)).bind(store.0).bind(hash_bytes(hash)?).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(())
    }

    async fn remove_location(&self, store: StoreId, hash: &BlobHash, backend: BackendId) -> Result<()> {
        sqlx::query(r(sql::REMOVE_LOCATION)).bind(store.0).bind(hash_bytes(hash)?).bind(backend.0).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(())
    }

    async fn pending_deletes(&self, store: StoreId, limit: u32) -> Result<Vec<(BlobHash, BackendId)>> {
        let rows = sqlx::query(r(sql::PENDING)).bind(store.0).bind(lim(limit)).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        let mut out = Vec::with_capacity(rows.len());
        for x in &rows {
            let bytes: Vec<u8> = x.try_get(0).map_err(map_db_error)?;
            let backend: i64 = x.try_get(1).map_err(map_db_error)?;
            if let Some(h) = hash_of(&bytes) {
                out.push((h, BackendId(backend)));
            }
        }
        Ok(out)
    }

    async fn under_replicated(&self, store: StoreId, copies: u32, after: Option<&BlobHash>, limit: u32) -> Result<Vec<BlobHash>> {
        let after: Option<Vec<u8>> = after.map(hash_bytes).transpose()?;
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(r(sql::UNDER_REPLICATED))
            .bind(store.0)
            .bind(i64::from(copies))
            .bind(after)
            .bind(lim(limit))
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(rows.iter().filter_map(|b| hash_of(b)).collect())
    }

    async fn held_by(&self, backend: BackendId, after: Option<&BlobHash>, limit: u32) -> Result<Vec<BlobHash>> {
        let after: Option<Vec<u8>> = after.map(hash_bytes).transpose()?;
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(r(sql::HELD_BY)).bind(backend.0).bind(after).bind(lim(limit)).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        Ok(rows.iter().filter_map(|b| hash_of(b)).collect())
    }

    async fn count_held(&self, backend: BackendId) -> Result<i64> {
        sqlx::query_scalar(r(sql::COUNT_HELD)).bind(backend.0).fetch_one(self.store.pool()).await.map_err(map_db_error)
    }
}
