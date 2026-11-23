//! `GcRepo` on Postgres. Blob rows are locked with `lock_existing_blobs` (ascending hash, the
//! store-wide rule) before they are deleted; see `repo/gc.rs` for the argument.

use async_trait::async_trait;
use rusty_core::{BlobHash, Error, MailboxId, Result};

use super::tx::tx_retry;
use super::{lock_existing_blobs, PgRepo};
use crate::map_db_error;
use crate::repo::gc::ClosedAccountBatch;
use crate::repo::{hash_bytes, GcRepo, MailboxRepo, PurgeBatch, StoreId};

/// Mailboxes visited by one `purge_deleted_batch` call.
const MAILBOXES_PER_BATCH: i64 = 16;

fn hash_of(bytes: &[u8]) -> Option<BlobHash> {
    BlobHash::parse(&hex::encode(bytes)).ok()
}

#[async_trait]
impl GcRepo for PgRepo {
    async fn acquire_gc_lease(&self, store: StoreId, holder: &str, ttl_secs: i64) -> Result<bool> {
        let done = sqlx::query(crate::repo::render_pg(
            "INSERT INTO blob_store_leases (store_id, holder, expires_at) VALUES ($1, $2, {NOW} + $3) \
             ON CONFLICT (store_id) DO UPDATE SET holder = EXCLUDED.holder, expires_at = EXCLUDED.expires_at \
             WHERE blob_store_leases.expires_at <= {NOW} OR blob_store_leases.holder = EXCLUDED.holder",
        ))
        .bind(store.0)
        .bind(holder)
        .bind(ttl_secs)
        .execute(self.store.pool())
        .await
        .map_err(map_db_error)?;
        Ok(done.rows_affected() == 1)
    }

    async fn release_gc_lease(&self, store: StoreId, holder: &str) -> Result<()> {
        sqlx::query(crate::repo::render_pg("DELETE FROM blob_store_leases WHERE store_id = $1 AND holder = $2"))
            .bind(store.0)
            .bind(holder)
            .execute(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    async fn db_now(&self) -> Result<i64> {
        sqlx::query_scalar(crate::repo::pg_lit("SELECT rs_now()"))
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)
    }

    async fn blob_candidates(
        &self,
        store: StoreId,
        cutoff_secs: i64,
        after: Option<&BlobHash>,
        limit: i64,
    ) -> Result<Vec<BlobHash>> {
        let after: Vec<u8> = match after {
            Some(h) => hash_bytes(h)?,
            None => Vec::new(),
        };
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(
            crate::repo::pg_lit("SELECT hash FROM blobs \
             WHERE store_id = $4 AND refcount = 0 AND zero_since < $1 AND hash > $2 ORDER BY hash LIMIT $3"),
        )
        .bind(cutoff_secs)
        .bind(after)
        .bind(limit)
        .bind(store.0)
        .fetch_all(self.store.pool())
        .await
        .map_err(map_db_error)?;
        Ok(rows.iter().filter_map(|b| hash_of(b)).collect())
    }

    async fn delete_blob_rows(&self, store: StoreId, hashes: &[BlobHash], cutoff_secs: i64) -> Result<Vec<BlobHash>> {
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
                    crate::repo::pg_lit("DELETE FROM blobs WHERE store_id = $3 AND hash = ANY($1) AND refcount = 0 \
                     AND zero_since IS NOT NULL AND zero_since < $2 RETURNING hash"),
                )
                .bind(&raw)
                .bind(cutoff_secs)
                .bind(store.0)
                .fetch_all(&mut *c)
                .await?;
                Ok(gone)
            }
        })?;
        let mut out: Vec<BlobHash> = deleted.iter().filter_map(|b| hash_of(b)).collect();
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(out)
    }

    async fn blob_rows_in_use_in_shard(&self, store: StoreId, shard: u8) -> Result<Vec<String>> {
        let mut lo = [0u8; 32];
        let mut hi = [0xffu8; 32];
        lo[0] = shard;
        hi[0] = shard;
        let rows: Vec<Vec<u8>> =
            sqlx::query_scalar(crate::repo::pg_lit("SELECT hash FROM blobs WHERE store_id = $3 AND hash >= $1 AND hash <= $2 AND refcount > 0"))
                .bind(lo.to_vec())
                .bind(hi.to_vec())
                .bind(store.0)
                .fetch_all(self.store.pool())
                .await
                .map_err(map_db_error)?;
        Ok(rows.iter().map(hex::encode).collect())
    }

    async fn blob_rows_in_shard(&self, store: StoreId, shard: u8) -> Result<Vec<String>> {

        let mut lo = [0u8; 32];
        let mut hi = [0xffu8; 32];
        lo[0] = shard;
        hi[0] = shard;
        let rows: Vec<Vec<u8>> =
            sqlx::query_scalar(crate::repo::pg_lit("SELECT hash FROM blobs WHERE store_id = $3 AND hash >= $1 AND hash <= $2"))
                .bind(lo.to_vec())
                .bind(hi.to_vec())
                .bind(store.0)
                .fetch_all(self.store.pool())
                .await
                .map_err(map_db_error)?;
        Ok(rows.iter().map(hex::encode).collect())
    }

    async fn blob_store_ids(&self) -> Result<Vec<StoreId>> {
        let ids: Vec<i64> = sqlx::query_scalar(crate::repo::pg_lit("SELECT id FROM blob_stores ORDER BY id"))
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(ids.into_iter().map(StoreId).collect())
    }

    async fn blob_row_exists(&self, store: StoreId, hash: &BlobHash) -> Result<bool> {
        sqlx::query_scalar(crate::repo::pg_lit("SELECT EXISTS (SELECT 1 FROM blobs WHERE store_id = $2 AND hash = $1)"))
            .bind(hash_bytes(hash)?)
            .bind(store.0)
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)
    }

    async fn expunge_by_retention(&self, now: i64, limit: i64) -> Result<u64> {
        let due = self.retention_due(now, limit).await?;
        let mut done = 0u64;
        let mut i = 0;
        while i < due.len() {
            let mb = due[i].0;
            let ids: Vec<i64> = due[i..].iter().take_while(|(m, _)| *m == mb).map(|(_, id)| *id).collect();
            i += ids.len();
            // One mailbox at a time, through the mailbox (the change log, so that an IMAP client sees the expunge); a busy one is left for the next pass.
            match MailboxRepo::expunge_many(self, MailboxId(mb), &ids, Some(now)).await {
                Ok(c) => done += u64::from(c.value.applied),
                Err(Error::Busy(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(done)
    }

    async fn purge_deleted_batch(&self, older_than: i64, limit: i64) -> Result<PurgeBatch> {
        let mut out = PurgeBatch::default();
        let mut visited = 0i64;
        let mut after = 0i64;
        loop {
            // Mailboxes that have something purgeable (same predicate as the per-mailbox purge,
            // so a mailbox held back by `deliveries` rows is never listed and cannot stall the
            // loop), a page at a time behind a keyset cursor: a mailbox that is busy is skipped
            // and the walk goes on behind it (review PG5 M-e).
            let boxes: Vec<i64> = sqlx::query_scalar(
                crate::repo::pg_lit("SELECT DISTINCT mailbox_id FROM messages m \
                 WHERE deleted_at IS NOT NULL AND deleted_at < $1 AND mailbox_id > $3 \
                 AND NOT EXISTS (SELECT 1 FROM deliveries d \
                   WHERE d.mailbox_id = m.mailbox_id AND d.message_id = m.id) \
                 ORDER BY mailbox_id LIMIT $2"),
            )
            .bind(older_than)
            .bind(MAILBOXES_PER_BATCH)
            .bind(after)
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)?;
            let page_full = boxes.len() as i64 >= MAILBOXES_PER_BATCH;
            for mb in boxes {
                after = mb;
                // Mailbox row first, blobs ascending: the one implementation of the rule.
                match MailboxRepo::purge_deleted(self, MailboxId(mb), older_than, limit).await {
                    Ok(c) => {
                        out.purged += c.value;
                        visited += 1;
                    }
                    // The mailbox vanished since the listing: nothing left to purge there.
                    Err(Error::NotFound(_)) => {}
                    Err(Error::Busy(_)) => out.busy_mailboxes.push(MailboxId(mb)),
                    Err(e) => return Err(e),
                }
            }
            if !page_full || visited >= MAILBOXES_PER_BATCH {
                return Ok(out);
            }
        }
    }

    async fn purge_deliveries_batch(&self, older_than: i64, limit: i64) -> Result<u64> {
        let n = tx_retry!(self, None, |c| {
            {
                Ok(sqlx::query(
                    crate::repo::pg_lit("DELETE FROM deliveries WHERE (mailbox_id, delivery_id) IN (\
                       SELECT mailbox_id, delivery_id FROM deliveries WHERE created_at < $1 \
                       ORDER BY created_at LIMIT $2 FOR UPDATE SKIP LOCKED)"),
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

    async fn purge_closed_account_batch(&self, older_than: i64, limit: i64) -> Result<ClosedAccountBatch> {
        let out = tx_retry!(self, None, |c| {
            {
                // The oldest due account that no other collector is working on.
                let due: Option<(i64, Option<i64>)> = sqlx::query_as(crate::repo::pg_lit(
                    "SELECT a.id, m.id FROM accounts a LEFT JOIN mailboxes m ON m.account_id = a.id 
                      WHERE a.status = 'disabled' AND a.closed_at IS NOT NULL AND a.closed_at < $1 ORDER BY a.closed_at, a.id LIMIT 1 FOR UPDATE OF a SKIP LOCKED",
                ))
                .bind(older_than)
                .fetch_optional(&mut *c)
                .await?;
                let Some((account, mailbox)) = due else { return Ok(ClosedAccountBatch::default()) };
                let mut messages = 0u64;
                if let Some(mb) = mailbox {
                    messages = sqlx::query(crate::repo::pg_lit(
                        "DELETE FROM messages WHERE (mailbox_id, id) IN (SELECT mailbox_id, id FROM messages WHERE mailbox_id = $1 ORDER BY id LIMIT $2)",
                    ))
                    .bind(mb)
                    .bind(limit)
                    .execute(&mut *c)
                    .await?
                    .rows_affected();
                }
                // Fewer than the limit were left: the mailbox is empty, the account can go.
                let mut accounts = 0u64;
                if messages < u64::try_from(limit).unwrap_or(0) {
                    accounts = sqlx::query(crate::repo::pg_lit("DELETE FROM accounts WHERE id = $1")).bind(account).execute(&mut *c).await?.rows_affected();
                }
                Ok(ClosedAccountBatch { messages, accounts, worked: true })
            }
        })?;
        Ok(out)
    }

    async fn purge_capabilities_batch(&self, older_than: i64, limit: i64) -> Result<u64> {
        let n = tx_retry!(self, None, |c| {
            {
                Ok(sqlx::query(
                    crate::repo::pg_lit("DELETE FROM capabilities WHERE token_hash IN (\
                       SELECT token_hash FROM capabilities \
                       WHERE expires_at < $1 OR revoked_at < $1 \
                       ORDER BY token_hash LIMIT $2 FOR UPDATE SKIP LOCKED)"),
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

/// The value of a retention key at one layer as a number of days (a text that is not a plain number is no value: a stored rule can never make the statement fail).
macro_rules! layer_days {
    ($l:literal, $k:literal) => {
        concat!("(CASE WHEN CAST(", $l, ".config AS jsonb) ->> '", $k, "' ~ '^[0-9]{1,5}$' THEN CAST(CAST(", $l, ".config AS jsonb) ->> '", $k, "' AS bigint) END)")
    };
}

/// The first layer that has the key: account, plan, domain, then the global one.
macro_rules! effective_days {
    ($k:literal) => {
        concat!("COALESCE(", layer_days!("a", $k), ", ", layer_days!("p", $k), ", ", layer_days!("d", $k), ", ", layer_days!("g", $k), ")")
    };
}

/// The messages a retention rule has made due: `spam_retention_days` for the Spam folder, `retention_days` for any folder, from the internal date. ($1 now, $2 limit, $3 the name of the global layer.)
const RETENTION_DUE: &str = concat!(
    "SELECT m.mailbox_id, m.id FROM messages m \
     JOIN mailboxes mb ON mb.id = m.mailbox_id \
     JOIN accounts a ON a.id = mb.account_id \
     JOIN domains d ON d.id = a.domain_id \
     LEFT JOIN plans p ON p.id = a.plan_id \
     LEFT JOIN plans g ON g.name = $3 \
     JOIN folders f ON f.mailbox_id = m.mailbox_id AND f.id = m.folder_id \
     WHERE m.deleted_at IS NULL AND ( \
       (f.role = 'spam' AND m.internal_date < $1 - 86400 * ",
    effective_days!("spam_retention_days"),
    ") OR m.internal_date < $1 - 86400 * ",
    effective_days!("retention_days"),
    ") ORDER BY m.mailbox_id, m.id LIMIT $2"
);

impl PgRepo {
    /// The messages that a retention rule has made due: (mailbox, id), in order, at most `limit`.
    async fn retention_due(&self, now: i64, limit: i64) -> Result<Vec<(i64, i64)>> {
        sqlx::query_as::<_, (i64, i64)>(crate::repo::pg_lit(RETENTION_DUE))
            .bind(now)
            .bind(limit)
            .bind(crate::repo::GLOBAL_PLAN)
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)
    }
}
