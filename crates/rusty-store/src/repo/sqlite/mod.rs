//! SQLite implementation of the repositories (directory: `directory.rs`; mailbox: spike, PG4
//! replaces it).
//!
//! Locking: `begin_locked` = `BEGIN IMMEDIATE` (global write lock) then touch the mailbox row,
//! exactly like `Mailbox::begin`. Counters are taken with one `UPDATE .. RETURNING` each.

mod directory;

use std::collections::HashMap;

use async_trait::async_trait;
use rusty_core::{
    AccountId, Error, FolderId, MailboxId, MessageId, Result, ThreadId,
};
use sqlx::{Sqlite, SqliteConnection, Transaction};

use super::{
    depth_verdict, not_found_folder, not_found_mailbox, render_sqlite as r, sql_text as q,
    thread_candidates, Delivery, MailboxRepo, NewMessage, JobFence,
};
use crate::mailbox::{
    to_flags, to_uid, ChangeItem, ChangeOp, ChangePage, DeliverOutcome, MessageRef,
    MessageSummary, MAX_FOLDER_DEPTH,
};
use crate::{map_db_error, SqliteStore};

#[derive(Clone, Debug)]
pub struct SqliteRepo {
    store: SqliteStore,
}

impl SqliteRepo {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    async fn begin_locked(&self, mb: MailboxId) -> Result<Transaction<'static, Sqlite>> {
        let mut tx = self.store.begin_write().await?;
        sqlx::query_scalar::<_, i64>("SELECT id FROM mailboxes WHERE id = ?1")
            .bind(mb.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_db_error)?
            .ok_or_else(not_found_mailbox)?;
        Ok(tx)
    }
}

/// Fence and complete the job in the caller's transaction (the `JobQueue::complete_in` pattern):
/// a stale epoch or expired lease is `Conflict` and the caller drops the transaction.
async fn complete_job(c: &mut SqliteConnection, f: JobFence) -> Result<()> {
    let n = sqlx::query(&r(q::COMPLETE_JOB))
        .bind(f.id.0)
        .bind(f.epoch)
        .execute(&mut *c)
        .await
        .map_err(map_db_error)?
        .rows_affected();
    if n == 1 {
        Ok(())
    } else {
        Err(Error::Conflict("stale job epoch".into()))
    }
}

async fn log_change(
    c: &mut SqliteConnection,
    mb: i64,
    item: ChangeItem,
    op: ChangeOp,
) -> Result<i64> {
    let (kind, item_id) = item.to_row();
    let seq: i64 = sqlx::query_scalar(&r(q::TAKE_SEQ))
        .bind(mb)
        .fetch_optional(&mut *c)
        .await
        .map_err(map_db_error)?
        .ok_or_else(not_found_mailbox)?;
    sqlx::query(&r(q::INSERT_CHANGE))
        .bind(mb)
        .bind(seq)
        .bind(kind)
        .bind(item_id)
        .bind(op.as_str())
        .execute(&mut *c)
        .await
        .map_err(map_db_error)?;
    Ok(seq)
}

async fn lookup_threads(
    c: &mut SqliteConnection,
    mb: i64,
    hdrs: &[&str],
) -> Result<HashMap<String, i64>> {
    // SQLite has no array parameter: one numbered placeholder per id (only numbers are
    // formatted into the text). Postgres uses `= ANY($2)`.
    let ph = (2..2 + hdrs.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT message_id_hdr, thread_id FROM messages WHERE mailbox_id = ?1 AND id IN ( \
           SELECT MIN(id) FROM messages WHERE mailbox_id = ?1 AND message_id_hdr IN ({ph}) \
           GROUP BY message_id_hdr)"
    );
    let mut qy = sqlx::query_as::<_, (String, i64)>(&sql).bind(mb);
    for h in hdrs {
        qy = qy.bind(*h);
    }
    Ok(qy
        .fetch_all(&mut *c)
        .await
        .map_err(map_db_error)?
        .into_iter()
        .collect())
}

/// Insert one message inside the caller's transaction. Returns the new ref.
async fn insert_message(
    c: &mut SqliteConnection,
    mb: i64,
    m: &NewMessage,
    delivery: Option<&Delivery>,
) -> Result<MessageRef> {
    sqlx::query("INSERT INTO blobs (hash, size) VALUES (?1, ?2) ON CONFLICT (hash) DO NOTHING")
        .bind(m.hash.as_str())
        .bind(m.size)
        .execute(&mut *c)
        .await
        .map_err(map_db_error)?;
    let uid_raw: i64 = sqlx::query_scalar(&r(q::TAKE_UIDS))
        .bind(mb)
        .bind(m.folder.0)
        .bind(1i64)
        .fetch_optional(&mut *c)
        .await
        .map_err(map_db_error)?
        .ok_or_else(not_found_folder)?;
    let uid = to_uid(uid_raw)?;
    let (id, seq): (i64, i64) = sqlx::query_as(&r(q::TAKE_MSG_IDS_AND_SEQS))
        .bind(mb)
        .bind(1i64)
        .fetch_optional(&mut *c)
        .await
        .map_err(map_db_error)?
        .ok_or_else(not_found_mailbox)?;
    let cands = thread_candidates(&m.env);
    let thread_id = if cands.is_empty() {
        id
    } else {
        let known = lookup_threads(c, mb, &cands).await?;
        cands.iter().find_map(|k| known.get(*k).copied()).unwrap_or(id)
    };
    sqlx::query(
        "INSERT INTO messages (mailbox_id, id, folder_id, uid, blob_hash, size, internal_date, \
         subject, from_addr, to_addrs, message_id_hdr, thread_id, flags) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch(), ?7, ?8, ?9, ?10, ?11, ?12)",
    )
    .bind(mb)
    .bind(id)
    .bind(m.folder.0)
    .bind(i64::from(uid))
    .bind(m.hash.as_str())
    .bind(m.size)
    .bind(&m.env.subject)
    .bind(&m.env.from)
    .bind(&m.env.to)
    .bind(m.env.message_id.as_deref())
    .bind(thread_id)
    .bind(i64::from(m.flags.bits()))
    .execute(&mut *c)
    .await
    .map_err(map_db_error)?;
    if let Some(d) = delivery {
        sqlx::query(
            "INSERT INTO deliveries (mailbox_id, delivery_id, message_id, blob_hash, created_at) \
             VALUES (?1, ?2, ?3, ?4, COALESCE(?5, unixepoch()))",
        )
        .bind(mb)
        .bind(&d.id.0[..])
        .bind(id)
        .bind(m.hash.as_str())
        .bind(d.created_at)
        .execute(&mut *c)
        .await
        .map_err(map_db_error)?;
    }
    sqlx::query(&r(q::INSERT_CHANGE))
        .bind(mb)
        .bind(seq)
        .bind("message")
        .bind(id)
        .bind("create")
        .execute(&mut *c)
        .await
        .map_err(map_db_error)?;
    Ok(MessageRef {
        id: MessageId(id),
        uid,
    })
}

#[async_trait]
impl MailboxRepo for SqliteRepo {
    async fn mailbox_for_account(&self, account: AccountId) -> Result<MailboxId> {
        sqlx::query_scalar::<_, i64>(&r(q::MAILBOX_FOR_ACCOUNT))
            .bind(account.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?
            .map(MailboxId)
            .ok_or_else(|| Error::NotFound("mailbox for account".into()))
    }

    async fn folder_exists(&self, mb: MailboxId, folder: FolderId) -> Result<()> {
        sqlx::query_scalar::<_, i64>(&r(q::FOLDER_EXISTS))
            .bind(mb.0)
            .bind(folder.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?
            .map(|_| ())
            .ok_or_else(not_found_folder)
    }

    async fn create_folder(
        &self,
        mb: MailboxId,
        parent: Option<FolderId>,
        name: &str,
    ) -> Result<FolderId> {
        let mut tx = self.begin_locked(mb).await?;
        if let Some(p) = parent {
            let rows: Vec<(i64, Option<i64>)> = sqlx::query_as(&r(q::ANCESTORS))
                .bind(mb.0)
                .bind(p.0)
                .bind(MAX_FOLDER_DEPTH as i64)
                .fetch_all(&mut *tx)
                .await
                .map_err(map_db_error)?;
            let depth = depth_verdict(&rows, None)?;
            if depth + 1 > MAX_FOLDER_DEPTH {
                return Err(Error::Invalid("folder tree too deep".into()));
            }
        }
        let uv: i64 = sqlx::query_scalar(
            "UPDATE mailboxes SET uid_validity = max(unixepoch(), uid_validity + 1) \
             WHERE id = ?1 RETURNING uid_validity",
        )
        .bind(mb.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_error)?;
        if uv > i64::from(u32::MAX) {
            return Err(Error::Quota("folder UIDVALIDITY space exhausted".into()));
        }
        let id: i64 = sqlx::query_scalar(&r(q::TAKE_FOLDER_ID))
            .bind(mb.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(map_db_error)?;
        sqlx::query(&r(q::INSERT_FOLDER))
            .bind(mb.0)
            .bind(id)
            .bind(parent.map(|p| p.0))
            .bind(name)
            .bind(uv)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        log_change(&mut tx, mb.0, ChangeItem::Folder(FolderId(id)), ChangeOp::Create).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(FolderId(id))
    }

    async fn append(
        &self,
        mb: MailboxId,
        msg: &NewMessage,
        delivery: Option<&Delivery>,
        fence: Option<JobFence>,
    ) -> Result<DeliverOutcome> {
        let mut tx = self.begin_locked(mb).await?;
        let mut duplicate = None;
        if let Some(d) = delivery {
            let hit: Option<(i64, String)> = sqlx::query_as(&r(q::DELIVERY_LOOKUP))
                .bind(mb.0)
                .bind(&d.id.0[..])
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_db_error)?;
            if let Some((msg_id, blob_hash)) = hit {
                if blob_hash != msg.hash.as_str() {
                    return Err(Error::Conflict(
                        "delivery id reused with different content".into(),
                    ));
                }
                let uid: i64 = sqlx::query_scalar(&r(q::MESSAGE_UID))
                    .bind(mb.0)
                    .bind(msg_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(map_db_error)?
                    .ok_or_else(|| Error::storage("delivery refers to a missing message"))?;
                duplicate = Some(MessageRef {
                    id: MessageId(msg_id),
                    uid: to_uid(uid)?,
                });
            }
        }
        let outcome = match duplicate {
            Some(r) => DeliverOutcome::Duplicate(r),
            None => DeliverOutcome::Created(insert_message(&mut tx, mb.0, msg, delivery).await?),
        };
        if let Some(f) = fence {
            complete_job(&mut tx, f).await?;
        }
        tx.commit().await.map_err(map_db_error)?;
        Ok(outcome)
    }

    async fn append_many(&self, mb: MailboxId, msgs: &[NewMessage]) -> Result<Vec<MessageRef>> {
        let mut tx = self.begin_locked(mb).await?;
        let mut out = Vec::with_capacity(msgs.len());
        for m in msgs {
            out.push(insert_message(&mut tx, mb.0, m, None).await?);
        }
        tx.commit().await.map_err(map_db_error)?;
        Ok(out)
    }

    async fn list_folder(
        &self,
        mb: MailboxId,
        folder: FolderId,
        after_uid: u32,
        limit: i64,
    ) -> Result<Vec<MessageSummary>> {
        self.folder_exists(mb, folder).await?;
        type Row = (i64, i64, i64, i64, String, String, i64, i64);
        let rows: Vec<Row> = sqlx::query_as(&r(q::LIST_FOLDER))
            .bind(mb.0)
            .bind(folder.0)
            .bind(i64::from(after_uid))
            .bind(limit)
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)?;
        rows.into_iter()
            .map(|(id, uid, size, internal_date, subject, from_addr, flags, thread)| {
                Ok(MessageSummary {
                    id: MessageId(id),
                    uid: to_uid(uid)?,
                    size,
                    internal_date,
                    subject,
                    from_addr,
                    flags: to_flags(flags)?,
                    thread_id: ThreadId(thread),
                })
            })
            .collect()
    }

    async fn changes_since(&self, mb: MailboxId, seq: i64, limit: i64) -> Result<ChangePage> {
        super::changes_since_generic(self.store.pool(), &r(q::CHANGES), mb, seq, limit).await
    }
}
