//! `GcRepo` on Postgres. Blob rows are locked with `lock_existing_blobs` (ascending hash, the
//! store-wide rule) before they are deleted; see `repo/gc.rs` for the argument.

use async_trait::async_trait;
use rusty_core::{BlobHash, Error, MailboxId, Result};

use super::tx::tx_retry;
use super::{lock_existing_blobs, PgRepo};
use crate::map_db_error;
use crate::repo::{hash_bytes, GcRepo, MailboxRepo};

/// Mailboxes visited by one `purge_deleted_batch` call.
const MAILBOXES_PER_BATCH: i64 = 16;

fn hash_of(bytes: &[u8]) -> Option<BlobHash> {
    BlobHash::parse(&hex::encode(bytes)).ok()
}

#[async_trait]
impl GcRepo for PgRepo {
    async fn db_now(&self) -> Result<i64> {
        sqlx::query_scalar("SELECT rs_now()")
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
        let after: Vec<u8> = match after {
            Some(h) => hash_bytes(h)?,
            None => Vec::new(),
        };
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT hash FROM blobs \
             WHERE refcount = 0 AND zero_since < $1 AND hash > $2 ORDER BY hash LIMIT $3",
        )
        .bind(cutoff_secs)
        .bind(after)
        .bind(limit)
        .fetch_all(self.store.pool())
        .await
        .map_err(map_db_error)?;
        Ok(rows.iter().filter_map(|b| hash_of(b)).collect())
    }

    async fn delete_blob_rows(&self, hashes: &[BlobHash], cutoff_secs: i64) -> Result<Vec<BlobHash>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let raw: Vec<Vec<u8>> = hashes.iter().map(hash_bytes).collect::<Result<_>>()?;
        let knobs = self.knobs;
        let deleted = tx_retry!(self, None, |c| {
            {
                // Ascending lock order first (the store-wide rule), then the delete re-checks
                // that each row is still unreferenced and past the cutoff.
                lock_existing_blobs(c, &raw, knobs).await?;
                let gone: Vec<Vec<u8>> = sqlx::query_scalar(
                    "DELETE FROM blobs WHERE hash = ANY($1) AND refcount = 0 \
                     AND zero_since IS NOT NULL AND zero_since < $2 RETURNING hash",
                )
                .bind(&raw)
                .bind(cutoff_secs)
                .fetch_all(&mut *c)
                .await?;
                Ok(gone)
            }
        })?;
        let mut out: Vec<BlobHash> = deleted.iter().filter_map(|b| hash_of(b)).collect();
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(out)
    }

    async fn blob_rows_in_shard(&self, shard: u8) -> Result<Vec<String>> {
        let mut lo = [0u8; 32];
        let mut hi = [0xffu8; 32];
        lo[0] = shard;
        hi[0] = shard;
        let rows: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT hash FROM blobs WHERE hash >= $1 AND hash <= $2")
                .bind(lo.to_vec())
                .bind(hi.to_vec())
                .fetch_all(self.store.pool())
                .await
                .map_err(map_db_error)?;
        Ok(rows.iter().map(hex::encode).collect())
    }

    async fn blob_row_exists(&self, hash: &BlobHash) -> Result<bool> {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM blobs WHERE hash = $1)")
            .bind(hash_bytes(hash)?)
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)
    }

    async fn purge_deleted_batch(&self, older_than: i64, limit: i64) -> Result<u64> {
        // Mailboxes that have something purgeable (same predicate as the per-mailbox purge, so
        // a mailbox held back by `deliveries` rows is never listed and cannot stall the loop).
        let boxes: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT mailbox_id FROM messages m \
             WHERE deleted_at IS NOT NULL AND deleted_at < $1 \
             AND NOT EXISTS (SELECT 1 FROM deliveries d \
               WHERE d.mailbox_id = m.mailbox_id AND d.message_id = m.id) \
             ORDER BY mailbox_id LIMIT $2",
        )
        .bind(older_than)
        .bind(MAILBOXES_PER_BATCH)
        .fetch_all(self.store.pool())
        .await
        .map_err(map_db_error)?;
        let mut total = 0u64;
        for mb in boxes {
            // Mailbox row first, blobs ascending: the one implementation of the rule.
            match MailboxRepo::purge_deleted(self, MailboxId(mb), older_than, limit).await {
                Ok(c) => total += c.value,
                // The mailbox vanished since the listing: nothing left to purge there.
                Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    async fn purge_deliveries_batch(&self, older_than: i64, limit: i64) -> Result<u64> {
        let n = tx_retry!(self, None, |c| {
            {
                Ok(sqlx::query(
                    "DELETE FROM deliveries WHERE (mailbox_id, delivery_id) IN (\
                       SELECT mailbox_id, delivery_id FROM deliveries WHERE created_at < $1 \
                       ORDER BY created_at LIMIT $2 FOR UPDATE SKIP LOCKED)",
                )
                .bind(older_than)
                .bind(limit)
                .execute(&mut *c)
                .await?
                .rows_affected())
            }
        })?;
        Ok(n)
    }

    async fn purge_capabilities_batch(&self, older_than: i64, limit: i64) -> Result<u64> {
        let n = tx_retry!(self, None, |c| {
            {
                Ok(sqlx::query(
                    "DELETE FROM capabilities WHERE token_hash IN (\
                       SELECT token_hash FROM capabilities \
                       WHERE expires_at < $1 OR revoked_at < $1 \
                       ORDER BY token_hash LIMIT $2 FOR UPDATE SKIP LOCKED)",
                )
                .bind(older_than)
                .bind(limit)
                .execute(&mut *c)
                .await?
                .rows_affected())
            }
        })?;
        Ok(n)
    }
}
