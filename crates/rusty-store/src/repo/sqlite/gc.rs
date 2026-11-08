//! `GcRepo` on SQLite (global write lock; the `refcount = 0` re-check and the RESTRICT foreign
//! key protect a referenced blob).

use async_trait::async_trait;
use rusty_core::{BlobHash, Result};

use super::SqliteRepo;
use crate::repo::{GcRepo, PurgeBatch};
use crate::map_db_error;

#[async_trait]
impl GcRepo for SqliteRepo {
    async fn db_now(&self) -> Result<i64> {
        sqlx::query_scalar(&crate::repo::sqlite_lit("SELECT unixepoch()"))
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)
    }

    async fn blob_candidates(
        &self,
        cutoff_secs: i64,
        after: Option<&BlobHash>,
        limit: i64,
    ) -> Result<Vec<BlobHash>> {
        let rows: Vec<String> = sqlx::query_scalar(
            &crate::repo::sqlite_lit("SELECT hash FROM blobs \
             WHERE refcount = 0 AND zero_since < ? AND hash > ? ORDER BY hash LIMIT ?"),
        )
        .bind(cutoff_secs)
        .bind(after.map_or("", |h| h.as_str()))
        .bind(limit)
        .fetch_all(self.store.pool())
        .await
        .map_err(map_db_error)?;
        // A row with an invalid hash cannot be handled; it is skipped (and never deleted).
        Ok(rows.iter().filter_map(|s| BlobHash::parse(s).ok()).collect())
    }

    async fn delete_blob_rows(&self, hashes: &[BlobHash], cutoff_secs: i64) -> Result<Vec<BlobHash>> {
        let mut tx = self.store.begin_write().await?;
        let mut deleted = Vec::new();
        for h in hashes {
            let n = sqlx::query(
                &crate::repo::sqlite_lit("DELETE FROM blobs WHERE hash = ? AND refcount = 0 \
                 AND zero_since IS NOT NULL AND zero_since < ?"),
            )
            .bind(h.as_str())
            .bind(cutoff_secs)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
            if n == 1 {
                deleted.push(h.clone());
            }
        }
        tx.commit().await.map_err(map_db_error)?;
        Ok(deleted)
    }

    async fn blob_rows_in_shard(&self, shard: u8) -> Result<Vec<String>> {
        // Every hash in this shard sorts in ["xy", "xyg"): hex digits are all below 'g'.
        sqlx::query_scalar(&crate::repo::sqlite_lit("SELECT hash FROM blobs WHERE hash >= ? AND hash < ?"))
            .bind(format!("{shard:02x}"))
            .bind(format!("{shard:02x}g"))
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)
    }

    async fn blob_row_exists(&self, hash: &BlobHash) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(&crate::repo::sqlite_lit("SELECT EXISTS (SELECT 1 FROM blobs WHERE hash = ?)"))
            .bind(hash.as_str())
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(n != 0)
    }

    async fn purge_deleted_batch(&self, older_than: i64, limit: i64) -> Result<PurgeBatch> {
        let mut tx = self.store.begin_write().await?;
        let n = sqlx::query(
            &crate::repo::sqlite_lit("DELETE FROM messages WHERE rowid IN (\
               SELECT rowid FROM messages \
               WHERE deleted_at IS NOT NULL AND deleted_at < ? \
               AND NOT EXISTS (SELECT 1 FROM deliveries d \
                 WHERE d.mailbox_id = messages.mailbox_id AND d.message_id = messages.id) \
               LIMIT ?)"),
        )
        .bind(older_than)
        .bind(limit)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?
        .rows_affected();
        tx.commit().await.map_err(map_db_error)?;
        Ok(PurgeBatch { purged: n, busy_mailboxes: Vec::new() })
    }

    async fn purge_deliveries_batch(&self, older_than: i64, limit: i64) -> Result<u64> {
        let mut tx = self.store.begin_write().await?;
        let n = sqlx::query(
            &crate::repo::sqlite_lit("DELETE FROM deliveries WHERE rowid IN (\
               SELECT rowid FROM deliveries WHERE created_at < ? LIMIT ?)"),
        )
        .bind(older_than)
        .bind(limit)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?
        .rows_affected();
        tx.commit().await.map_err(map_db_error)?;
        Ok(n)
    }

    async fn purge_capabilities_batch(&self, older_than: i64, limit: i64) -> Result<u64> {
        let mut tx = self.store.begin_write().await?;
        let n = sqlx::query(
            &crate::repo::sqlite_lit("DELETE FROM capabilities WHERE rowid IN (\
               SELECT rowid FROM capabilities \
               WHERE expires_at < ?1 OR revoked_at < ?1 LIMIT ?2)"),
        )
        .bind(older_than)
        .bind(limit)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?
        .rows_affected();
        tx.commit().await.map_err(map_db_error)?;
        Ok(n)
    }
}
