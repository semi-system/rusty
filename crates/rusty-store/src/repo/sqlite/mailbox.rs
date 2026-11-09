//! `MailboxRepo` on SQLite. Every write is `begin_locked` (`BEGIN IMMEDIATE` + the mailbox row
//! first), set-based statements with dynamic `IN (...)` lists of at most `SQL_CHUNK` numbered
//! placeholders (only NUMBERS are ever formatted into statement text, caller data is always
//! bound), counters taken with one `UPDATE .. RETURNING` each, one commit.
//!
//! SQLite has a single global writer, so the blob-lock ordering rule of the Postgres
//! implementation is vacuous here; the statements are still issued in the same logical order.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use rusty_core::{
    AccountId, BlobHash, Error, Flags, FolderId, MailboxId, MessageId, Result,
};
use sqlx::SqliteConnection;

use super::SqliteRepo;
use crate::mailbox::{
    to_uid, BatchOutcome, ChangeItem, ChangeOp, ChangePage, CopyBatch, DeliverOutcome, MessageRef,
    MessageSummary, MoveBatch, MAX_FOLDER_DEPTH, MAX_KEYWORDS_PER_MESSAGE,
};
use crate::repo::{
    changes_since_generic, check_batch_len, check_distinct_ids, check_purge_limit, depth_verdict, moved_refs, not_found_folder,
    not_found_mailbox, not_found_message, plan_deliveries, plan_move, render_sqlite as r,
    split_hits, sql_text as q, summary_from_row, thread_candidates, Committed, DeliverEntry,
    Delivery, FolderCounts, FolderInfo, JobFence, MailboxRepo, MessageDetail, NewMessage, RawRef, Slot,
    counts_from_row, folder_from_row, FolderRow,
    SummaryRow,
};
use crate::map_db_error;

/// Most bound ids per statement in the set-based queries (SQLite's default limit is 32,766
/// variables; this stays far below it).
const SQL_CHUNK: usize = 500;
/// Rows per multi-row INSERT into `change_log` (2 parameters per row).
const LOG_CHUNK: usize = 400;

/// `?first,?first+1,..` for `n` numbered parameters. Only numbers are formatted into SQL text.
fn placeholders(first: usize, n: usize) -> String {
    (first..first + n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

// ---------------------------------------------------------------- in-transaction primitives

/// Fence and complete the job in the caller's transaction (the `JobQueue::complete_in` pattern):
/// a stale epoch or expired lease is `Conflict` and the caller drops the transaction.
/// A job that has a domain may only act on a mailbox of that domain (`Invalid` otherwise, review
/// PG5 M-c).
async fn complete_job(c: &mut SqliteConnection, f: JobFence, mailbox: i64) -> Result<()> {
    let n = sqlx::query(&r(q::COMPLETE_JOB))
        .bind(f.id.0)
        .bind(f.epoch)
        .bind(mailbox)
        .execute(&mut *c)
        .await
        .map_err(map_db_error)?
        .rows_affected();
    if n == 1 {
        return Ok(());
    }
    let owned: Option<i64> = sqlx::query_scalar(&r(q::JOB_STILL_OWNED))
        .bind(f.id.0)
        .bind(f.epoch)
        .fetch_optional(&mut *c)
        .await
        .map_err(map_db_error)?;
    if owned.is_some() {
        Err(Error::Invalid("job belongs to another domain".into()))
    } else {
        Err(Error::Conflict("stale job epoch".into()))
    }
}

/// Append a change_log row with the next seq of the mailbox; returns that seq.
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

/// Log one `op` change per message id, in order: the seq range is allocated with a single
/// `UPDATE ... RETURNING` (no gaps) and the rows go in with multi-row INSERTs. Returns the last
/// seq (`None` for an empty list).
async fn log_message_changes(
    c: &mut SqliteConnection,
    mb: i64,
    ids: &[i64],
    op: ChangeOp,
) -> Result<Option<i64>> {
    if ids.is_empty() {
        return Ok(None);
    }
    let n = i64::try_from(ids.len()).map_err(|_| Error::Invalid("batch too large".into()))?;
    let first: i64 = sqlx::query_scalar(&r(q::TAKE_SEQS))
        .bind(mb)
        .bind(n)
        .fetch_optional(&mut *c)
        .await
        .map_err(map_db_error)?
        .ok_or_else(not_found_mailbox)?;
    let mut seq = first;
    for chunk in ids.chunks(LOG_CHUNK) {
        // `op` is one of three constants, never caller input.
        let rows = (0..chunk.len())
            .map(|r| {
                format!(
                    "(?1, ?{}, 'message', ?{}, '{}')",
                    2 + 2 * r,
                    3 + 2 * r,
                    op.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let sql =
            format!("INSERT INTO change_log (mailbox_id, seq, kind, item_id, op) VALUES {rows}");
        let mut qy = sqlx::query(&sql).bind(mb);
        for id in chunk {
            qy = qy.bind(seq).bind(*id);
            seq += 1;
        }
        qy.execute(&mut *c).await.map_err(map_db_error)?;
    }
    Ok(Some(first + n - 1))
}

/// Issue a new folder UIDVALIDITY: `max(DB clock, high_water + 1)`, advancing the mailbox
/// high-water mark in the same statement. Past `u32::MAX` it is `Quota` (the caller's
/// transaction rolls back and the mark is unchanged).
async fn take_uid_validity(c: &mut SqliteConnection, mb: i64) -> Result<i64> {
    let uv: i64 = sqlx::query_scalar(
        &crate::repo::sqlite_lit("UPDATE mailboxes SET uid_validity = max(unixepoch(), uid_validity + 1) \
         WHERE id = ?1 RETURNING uid_validity"),
    )
    .bind(mb)
    .fetch_optional(&mut *c)
    .await
    .map_err(map_db_error)?
    .ok_or_else(not_found_mailbox)?;
    if uv > i64::from(u32::MAX) {
        return Err(Error::Quota("folder UIDVALIDITY space exhausted".into()));
    }
    Ok(uv)
}

/// Take `n` uids of `folder` (NotFound if the folder is not in this mailbox); returns the first.
async fn take_uids(c: &mut SqliteConnection, mb: i64, folder: i64, n: i64) -> Result<i64> {
    sqlx::query_scalar(&r(q::TAKE_UIDS))
        .bind(mb)
        .bind(folder)
        .bind(n)
        .fetch_optional(&mut *c)
        .await
        .map_err(map_db_error)?
        .ok_or_else(not_found_folder)
}

/// `(parent_id, role)` of a folder of this mailbox.
async fn folder_row(
    c: &mut SqliteConnection,
    mb: i64,
    id: i64,
) -> Result<(Option<i64>, String)> {
    sqlx::query_as(&r(q::FOLDER_ROW))
        .bind(mb)
        .bind(id)
        .fetch_optional(&mut *c)
        .await
        .map_err(map_db_error)?
        .ok_or_else(not_found_folder)
}

/// `Invalid` for system folders (role other than `custom`); returns the parent.
async fn custom_folder(c: &mut SqliteConnection, mb: i64, id: i64) -> Result<Option<i64>> {
    let (parent, role) = folder_row(c, mb, id).await?;
    if role != "custom" {
        return Err(Error::Invalid("system folders cannot be changed".into()));
    }
    Ok(parent)
}

/// Number of levels from `start` up to its root (start included); see `depth_verdict`.
async fn ancestor_depth(
    c: &mut SqliteConnection,
    mb: i64,
    start: i64,
    forbidden: Option<i64>,
) -> Result<usize> {
    let rows: Vec<(i64, Option<i64>)> = sqlx::query_as(&r(q::ANCESTORS))
        .bind(mb)
        .bind(start)
        .bind(MAX_FOLDER_DEPTH as i64)
        .fetch_all(&mut *c)
        .await
        .map_err(map_db_error)?;
    depth_verdict(&rows, forbidden)
}

/// Levels in the subtree rooted at `id` (a leaf is 1), capped just above the limit.
async fn subtree_height(c: &mut SqliteConnection, mb: i64, id: i64) -> Result<usize> {
    let h: i64 = sqlx::query_scalar(&r(q::SUBTREE_HEIGHT))
        .bind(id)
        .bind(mb)
        .bind(MAX_FOLDER_DEPTH as i64)
        .fetch_one(&mut *c)
        .await
        .map_err(map_db_error)?;
    usize::try_from(h).map_err(|_| Error::storage("corrupt folder tree"))
}

/// A live (not soft-deleted) message of this mailbox: `(folder_id, uid)`.
async fn live_message(c: &mut SqliteConnection, mb: i64, id: i64) -> Result<(i64, i64)> {
    sqlx::query_as(&r(q::LIVE_MESSAGE))
        .bind(mb)
        .bind(id)
        .fetch_optional(&mut *c)
        .await
        .map_err(map_db_error)?
        .ok_or_else(not_found_message)
}

/// Thread of the oldest message (smallest id) for each of the given Message-IDs, in one
/// statement per `SQL_CHUNK` ids (one for the at most 32 thread candidates). Soft-deleted
/// messages count.
async fn lookup_threads(
    c: &mut SqliteConnection,
    mb: i64,
    hdrs: &[&str],
) -> Result<HashMap<String, i64>> {
    let mut out = HashMap::new();
    for chunk in hdrs.chunks(SQL_CHUNK) {
        let sql = format!(
            "SELECT message_id_hdr, thread_id FROM messages WHERE mailbox_id = ?1 AND id IN ( \
               SELECT MIN(id) FROM messages WHERE mailbox_id = ?1 AND message_id_hdr IN ({}) \
               GROUP BY message_id_hdr)",
            placeholders(2, chunk.len())
        );
        let mut qy = sqlx::query_as::<_, (String, i64)>(&sql).bind(mb);
        for h in chunk {
            qy = qy.bind(*h);
        }
        out.extend(qy.fetch_all(&mut *c).await.map_err(map_db_error)?);
    }
    Ok(out)
}

/// Insert one message inside the caller's transaction (the mailbox row is locked).
async fn insert_message(
    c: &mut SqliteConnection,
    mb: i64,
    m: &NewMessage,
    delivery: Option<&Delivery>,
) -> Result<(MessageRef, i64)> {
    sqlx::query(&crate::repo::sqlite_lit("INSERT INTO blobs (hash, size) VALUES (?1, ?2) ON CONFLICT (hash) DO NOTHING"))
        .bind(m.hash.as_str())
        .bind(m.size)
        .execute(&mut *c)
        .await
        .map_err(map_db_error)?;
    let uid = to_uid(take_uids(c, mb, m.folder.0, 1).await?)?;
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
    // internal_date: the database clock, or the caller's `_at(now)` value verbatim (D2).
    let at = delivery.and_then(|d| d.created_at).or(m.internal_date);
    sqlx::query(
        &crate::repo::sqlite_lit("INSERT INTO messages (mailbox_id, id, folder_id, uid, blob_hash, size, internal_date, \
         subject, from_addr, to_addrs, message_id_hdr, thread_id, flags) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(?13, unixepoch()), ?7, ?8, ?9, ?10, ?11, ?12)"),
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
    .bind(at)
    .execute(&mut *c)
    .await
    .map_err(map_db_error)?;
    if let Some(d) = delivery {
        sqlx::query(
            &crate::repo::sqlite_lit("INSERT INTO deliveries (mailbox_id, delivery_id, message_id, blob_hash, created_at) \
             VALUES (?1, ?2, ?3, ?4, COALESCE(?5, unixepoch()))"),
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
    Ok((
        MessageRef {
            id: MessageId(id),
            uid,
        },
        seq,
    ))
}

type DetailRow = (i64, i64, i64, i64, String, String, i64, i64, i64, String, Option<i64>);

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

    async fn list_folders(&self, mb: MailboxId) -> Result<Vec<FolderInfo>> {
        let rows: Vec<FolderRow> = sqlx::query_as(&r(q::LIST_FOLDERS))
            .bind(mb.0)
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)?;
        rows.into_iter().map(folder_from_row).collect()
    }

    async fn folder_counts(&self, mb: MailboxId, folder: FolderId) -> Result<FolderCounts> {
        self.folder_exists(mb, folder).await?;
        let row: (i64, i64) = sqlx::query_as(&r(q::FOLDER_COUNTS))
            .bind(mb.0)
            .bind(folder.0)
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)?;
        counts_from_row(row)
    }

    async fn set_subscribed(
        &self,
        mb: MailboxId,
        id: FolderId,
        subscribed: bool,
    ) -> Result<Committed<()>> {
        let mut tx = self.begin_locked(mb).await?;
        let n = sqlx::query(&r(q::SET_SUBSCRIBED))
            .bind(mb.0)
            .bind(id.0)
            .bind(i64::from(subscribed))
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
        if n == 0 {
            return Err(not_found_folder());
        }
        let seq = log_change(&mut tx, mb.0, ChangeItem::Folder(id), ChangeOp::Update).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn create_folder(
        &self,
        mb: MailboxId,
        parent: Option<FolderId>,
        name: &str,
    ) -> Result<Committed<FolderId>> {
        let mut tx = self.begin_locked(mb).await?;
        if let Some(p) = parent {
            let depth = ancestor_depth(&mut tx, mb.0, p.0, None).await?;
            if depth + 1 > MAX_FOLDER_DEPTH {
                return Err(Error::Invalid("folder tree too deep".into()));
            }
        }
        let uv = take_uid_validity(&mut tx, mb.0).await?;
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
        let seq = log_change(&mut tx, mb.0, ChangeItem::Folder(FolderId(id)), ChangeOp::Create).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new(FolderId(id), Some(seq)))
    }

    async fn rename_folder(
        &self,
        mb: MailboxId,
        id: FolderId,
        name: &str,
    ) -> Result<Committed<()>> {
        let mut tx = self.begin_locked(mb).await?;
        custom_folder(&mut tx, mb.0, id.0).await?;
        sqlx::query(&r(q::RENAME_FOLDER))
            .bind(mb.0)
            .bind(id.0)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        let seq = log_change(&mut tx, mb.0, ChangeItem::Folder(id), ChangeOp::Update).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn move_folder(
        &self,
        mb: MailboxId,
        id: FolderId,
        new_parent: Option<FolderId>,
    ) -> Result<Committed<()>> {
        let mut tx = self.begin_locked(mb).await?;
        let old_parent = custom_folder(&mut tx, mb.0, id.0).await?;
        if old_parent == new_parent.map(|p| p.0) {
            return Ok(Committed::new((), None));
        }
        if let Some(p) = new_parent {
            let depth = ancestor_depth(&mut tx, mb.0, p.0, Some(id.0)).await?;
            let height = subtree_height(&mut tx, mb.0, id.0).await?;
            if depth + height > MAX_FOLDER_DEPTH {
                return Err(Error::Invalid("folder tree too deep".into()));
            }
        }
        sqlx::query(&r(q::REPARENT_FOLDER))
            .bind(mb.0)
            .bind(id.0)
            .bind(new_parent.map(|p| p.0))
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        let seq = log_change(&mut tx, mb.0, ChangeItem::Folder(id), ChangeOp::Update).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn delete_folder(&self, mb: MailboxId, id: FolderId) -> Result<Committed<()>> {
        let mut tx = self.begin_locked(mb).await?;
        custom_folder(&mut tx, mb.0, id.0).await?;
        let children: i64 = sqlx::query_scalar(&r(q::CHILD_COUNT))
            .bind(mb.0)
            .bind(id.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(map_db_error)?;
        if children > 0 {
            return Err(Error::Conflict("folder has subfolders".into()));
        }
        let live: i64 = sqlx::query_scalar(&r(q::LIVE_COUNT))
            .bind(mb.0)
            .bind(id.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(map_db_error)?;
        if live > 0 {
            return Err(Error::Conflict("folder still contains messages".into()));
        }
        sqlx::query(&r(q::DELETE_FOLDER_MESSAGES))
            .bind(mb.0)
            .bind(id.0)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        sqlx::query(&r(q::DELETE_FOLDER))
            .bind(mb.0)
            .bind(id.0)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        let seq = log_change(&mut tx, mb.0, ChangeItem::Folder(id), ChangeOp::Destroy).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn append(
        &self,
        mb: MailboxId,
        msg: &NewMessage,
        delivery: Option<&Delivery>,
        fence: Option<JobFence>,
    ) -> Result<Committed<DeliverOutcome>> {
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
        let (outcome, seq) = match duplicate {
            Some(r) => (DeliverOutcome::Duplicate(r), None),
            None => {
                let (r, seq) = insert_message(&mut tx, mb.0, msg, delivery).await?;
                (DeliverOutcome::Created(r), Some(seq))
            }
        };
        if let Some(f) = fence {
            complete_job(&mut tx, f, mb.0).await?;
        }
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new(outcome, seq))
    }

    async fn append_many(
        &self,
        mb: MailboxId,
        msgs: &[NewMessage],
    ) -> Result<Committed<Vec<MessageRef>>> {
        check_batch_len(msgs.len())?;
        let mut tx = self.begin_locked(mb).await?;
        let mut out = Vec::with_capacity(msgs.len());
        let mut last = None;
        for m in msgs {
            let (r, seq) = insert_message(&mut tx, mb.0, m, None).await?;
            out.push(r);
            last = Some(seq);
        }
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new(out, last))
    }

    async fn deliver_many(
        &self,
        mb: MailboxId,
        items: &[DeliverEntry],
        at: Option<i64>,
        fence: Option<JobFence>,
    ) -> Result<Committed<Vec<DeliverOutcome>>> {
        check_batch_len(items.len())?;
        let folder = items[0].msg.folder;
        if items.iter().any(|i| i.msg.folder != folder) {
            return Err(Error::Invalid("a delivery batch targets one folder".into()));
        }
        let mut tx = self.begin_locked(mb).await?;
        // Deliveries that already exist (one statement per SQL_CHUNK distinct ids).
        let mut wanted: Vec<&[u8]> = Vec::new();
        let mut seen = HashSet::new();
        for it in items {
            if seen.insert(it.delivery_id.0) {
                wanted.push(&it.delivery_id.0[..]);
            }
        }
        let mut existing: HashMap<Vec<u8>, (i64, String)> = HashMap::new();
        for chunk in wanted.chunks(SQL_CHUNK) {
            let sql = format!(
                "SELECT delivery_id, message_id, blob_hash FROM deliveries \
                 WHERE mailbox_id = ?1 AND delivery_id IN ({})",
                placeholders(2, chunk.len())
            );
            let mut qy = sqlx::query_as::<_, (Vec<u8>, i64, String)>(&sql).bind(mb.0);
            for d in chunk {
                qy = qy.bind(*d);
            }
            for (d, msg, hash) in qy.fetch_all(&mut *tx).await.map_err(map_db_error)? {
                existing.insert(d, (msg, hash));
            }
        }
        let plan = plan_deliveries(items, &existing)?;
        // Uids of the messages earlier deliveries created.
        let old_ids: Vec<i64> = {
            let mut s = HashSet::new();
            plan.slots
                .iter()
                .filter_map(|s| match s {
                    Slot::Existing(m) => Some(*m),
                    _ => None,
                })
                .filter(|m| s.insert(*m))
                .collect()
        };
        let mut old_uid: HashMap<i64, i64> = HashMap::new();
        for chunk in old_ids.chunks(SQL_CHUNK) {
            let sql = format!(
                "SELECT id, uid FROM messages WHERE mailbox_id = ?1 AND id IN ({})",
                placeholders(2, chunk.len())
            );
            let mut qy = sqlx::query_as::<_, (i64, i64)>(&sql).bind(mb.0);
            for m in chunk {
                qy = qy.bind(*m);
            }
            old_uid.extend(qy.fetch_all(&mut *tx).await.map_err(map_db_error)?);
        }

        let creates = &plan.creates;
        let mut first_id = 0i64;
        let mut first_uid = 0i64;
        let mut last_seq = None;
        if !creates.is_empty() {
            let n = creates.len() as i64;
            first_id = sqlx::query_scalar(
                &crate::repo::sqlite_lit("UPDATE mailboxes SET next_msg_id = next_msg_id + ?1 WHERE id = ?2 \
                 RETURNING next_msg_id - ?1"),
            )
            .bind(n)
            .bind(mb.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_db_error)?
            .ok_or_else(not_found_mailbox)?;
            first_uid = take_uids(&mut tx, mb.0, folder.0, n).await?;
            to_uid(first_uid + n - 1)?; // the last uid must still fit

            // Threads: one lookup for every candidate of every new message against the rows
            // that exist before this batch (all with smaller ids than any new message), then
            // earlier messages of this batch, candidate by candidate, as a loop would see.
            let cands: Vec<Vec<&str>> = creates
                .iter()
                .map(|&i| thread_candidates(&items[i].msg.env))
                .collect();
            let mut all: Vec<&str> = cands.iter().flatten().copied().collect();
            all.sort_unstable();
            all.dedup();
            let in_db = lookup_threads(&mut tx, mb.0, &all).await?;
            let mut in_batch: HashMap<&str, i64> = HashMap::new();
            let mut threads = Vec::with_capacity(creates.len());
            for (c, &i) in creates.iter().enumerate() {
                let id = first_id + c as i64;
                let thread = cands[c]
                    .iter()
                    .find_map(|h| in_db.get(*h).or_else(|| in_batch.get(h)).copied())
                    .unwrap_or(id);
                threads.push(thread);
                if let Some(mid) = items[i].msg.env.message_id.as_deref() {
                    in_batch.entry(mid).or_insert(thread);
                }
            }

            for (ci, chunk) in creates.chunks(60).enumerate() {
                let first_c = ci * 60;
                // blobs rows (refcount is maintained by the messages triggers)
                let rows = (0..chunk.len())
                    .map(|r| format!("(?{}, ?{})", 1 + 2 * r, 2 + 2 * r))
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "INSERT INTO blobs (hash, size) VALUES {rows} ON CONFLICT (hash) DO NOTHING"
                );
                let mut qy = sqlx::query(&sql);
                for &i in chunk {
                    qy = qy.bind(items[i].msg.hash.as_str()).bind(items[i].msg.size);
                }
                qy.execute(&mut *tx).await.map_err(map_db_error)?;

                // messages
                let rows = (0..chunk.len())
                    .map(|r| {
                        let b = 4 + 10 * r;
                        format!(
                            "(?1, ?{}, ?3, ?{}, ?{}, ?{}, COALESCE(?2, unixepoch()), ?{}, ?{}, ?{}, ?{}, ?{}, ?{})",
                            b, b + 1, b + 2, b + 3, b + 4, b + 5, b + 6, b + 7, b + 8, b + 9
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "INSERT INTO messages (mailbox_id, id, folder_id, uid, blob_hash, size, \
                     internal_date, subject, from_addr, to_addrs, message_id_hdr, thread_id, \
                     flags) VALUES {rows}"
                );
                let mut qy = sqlx::query(&sql).bind(mb.0).bind(at).bind(folder.0);
                for (k, &i) in chunk.iter().enumerate() {
                    let c = first_c + k;
                    let m = &items[i].msg;
                    qy = qy
                        .bind(first_id + c as i64)
                        .bind(first_uid + c as i64)
                        .bind(m.hash.as_str())
                        .bind(m.size)
                        .bind(&m.env.subject)
                        .bind(&m.env.from)
                        .bind(&m.env.to)
                        .bind(m.env.message_id.as_deref())
                        .bind(threads[c])
                        .bind(i64::from(m.flags.bits()));
                }
                qy.execute(&mut *tx).await.map_err(map_db_error)?;

                // deliveries
                let rows = (0..chunk.len())
                    .map(|r| {
                        let b = 3 + 3 * r;
                        format!(
                            "(?1, ?{}, ?{}, ?{}, COALESCE(?2, unixepoch()))",
                            b,
                            b + 1,
                            b + 2
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "INSERT INTO deliveries (mailbox_id, delivery_id, message_id, blob_hash, \
                     created_at) VALUES {rows}"
                );
                let mut qy = sqlx::query(&sql).bind(mb.0).bind(at);
                for (k, &i) in chunk.iter().enumerate() {
                    qy = qy
                        .bind(&items[i].delivery_id.0[..])
                        .bind(first_id + (first_c + k) as i64)
                        .bind(items[i].msg.hash.as_str());
                }
                qy.execute(&mut *tx).await.map_err(map_db_error)?;
            }
            let created_ids: Vec<i64> = (0..creates.len()).map(|c| first_id + c as i64).collect();
            last_seq = log_message_changes(&mut tx, mb.0, &created_ids, ChangeOp::Create).await?;
        }
        if let Some(f) = fence {
            complete_job(&mut tx, f, mb.0).await?;
        }

        let new_ref = |c: usize| -> Result<MessageRef> {
            Ok(MessageRef {
                id: MessageId(first_id + c as i64),
                uid: to_uid(first_uid + c as i64)?,
            })
        };
        let outcomes = plan
            .slots
            .iter()
            .map(|s| match *s {
                Slot::Created(c) => Ok(DeliverOutcome::Created(new_ref(c)?)),
                Slot::DuplicateOfNew(c) => Ok(DeliverOutcome::Duplicate(new_ref(c)?)),
                Slot::Existing(m) => {
                    let uid = old_uid
                        .get(&m)
                        .copied()
                        .ok_or_else(|| Error::storage("delivery refers to a missing message"))?;
                    Ok(DeliverOutcome::Duplicate(MessageRef {
                        id: MessageId(m),
                        uid: to_uid(uid)?,
                    }))
                }
            })
            .collect::<Result<Vec<_>>>()?;
        // Every fallible step is done: nothing can fail after the commit.
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new(outcomes, last_seq))
    }

    async fn set_flags(&self, mb: MailboxId, id: MessageId, flags: Flags) -> Result<Committed<()>> {
        let mut tx = self.begin_locked(mb).await?;
        let n = sqlx::query(&r(q::SET_FLAGS))
            .bind(mb.0)
            .bind(id.0)
            .bind(i64::from(flags.bits()))
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
        if n == 0 {
            return Err(not_found_message());
        }
        let seq = log_change(&mut tx, mb.0, ChangeItem::Message(id), ChangeOp::Update).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn set_flags_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        flags: Flags,
    ) -> Result<Committed<BatchOutcome>> {
        check_batch_len(ids.len())?;
        let mut tx = self.begin_locked(mb).await?;
        let mut hit = HashSet::with_capacity(ids.len());
        for chunk in ids.chunks(SQL_CHUNK) {
            let sql = format!(
                "UPDATE messages SET flags = ?1 WHERE mailbox_id = ?2 AND deleted_at IS NULL \
                 AND id IN ({}) RETURNING id",
                placeholders(3, chunk.len())
            );
            let mut qy = sqlx::query_scalar::<_, i64>(&sql)
                .bind(i64::from(flags.bits()))
                .bind(mb.0);
            for id in chunk {
                qy = qy.bind(*id);
            }
            hit.extend(qy.fetch_all(&mut *tx).await.map_err(map_db_error)?);
        }
        let (applied, missing) = split_hits(ids, &hit);
        let seq = log_message_changes(&mut tx, mb.0, &applied, ChangeOp::Update).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new(
            BatchOutcome {
                applied: applied.len() as u32,
                missing,
            },
            seq,
        ))
    }

    async fn add_keyword(
        &self,
        mb: MailboxId,
        id: MessageId,
        keyword: &str,
    ) -> Result<Committed<()>> {
        let mut tx = self.begin_locked(mb).await?;
        live_message(&mut tx, mb.0, id.0).await?;
        let present: Option<i64> = sqlx::query_scalar(&r(q::KEYWORD_PRESENT))
            .bind(mb.0)
            .bind(id.0)
            .bind(keyword)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_db_error)?;
        if present.is_some() {
            return Ok(Committed::new((), None));
        }
        let count: i64 = sqlx::query_scalar(&r(q::KEYWORD_COUNT))
            .bind(mb.0)
            .bind(id.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(map_db_error)?;
        if usize::try_from(count).map_or(true, |c| c >= MAX_KEYWORDS_PER_MESSAGE) {
            return Err(Error::Quota("too many keywords on message".into()));
        }
        sqlx::query(&r(q::KEYWORD_INSERT))
            .bind(mb.0)
            .bind(id.0)
            .bind(keyword)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        let seq = log_change(&mut tx, mb.0, ChangeItem::Message(id), ChangeOp::Update).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn remove_keyword(
        &self,
        mb: MailboxId,
        id: MessageId,
        keyword: &str,
    ) -> Result<Committed<()>> {
        let mut tx = self.begin_locked(mb).await?;
        live_message(&mut tx, mb.0, id.0).await?;
        let n = sqlx::query(&r(q::KEYWORD_DELETE))
            .bind(mb.0)
            .bind(id.0)
            .bind(keyword)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
        if n == 0 {
            return Ok(Committed::new((), None));
        }
        let seq = log_change(&mut tx, mb.0, ChangeItem::Message(id), ChangeOp::Update).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn move_message(
        &self,
        mb: MailboxId,
        id: MessageId,
        to: FolderId,
    ) -> Result<Committed<MessageRef>> {
        let mut tx = self.begin_locked(mb).await?;
        let (folder, uid) = live_message(&mut tx, mb.0, id.0).await?;
        if folder == to.0 {
            return Ok(Committed::new(
                MessageRef {
                    id,
                    uid: to_uid(uid)?,
                },
                None,
            ));
        }
        let new_uid = to_uid(take_uids(&mut tx, mb.0, to.0, 1).await?)?;
        sqlx::query(&r(q::MOVE_MESSAGE))
            .bind(mb.0)
            .bind(id.0)
            .bind(to.0)
            .bind(i64::from(new_uid))
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        let seq = log_change(&mut tx, mb.0, ChangeItem::Message(id), ChangeOp::Update).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new(MessageRef { id, uid: new_uid }, Some(seq)))
    }

    async fn move_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        to: FolderId,
    ) -> Result<Committed<MoveBatch>> {
        check_batch_len(ids.len())?;
        check_distinct_ids(ids)?;
        let mut tx = self.begin_locked(mb).await?;
        folder_row(&mut tx, mb.0, to.0).await?;
        let mut live: HashMap<i64, (i64, i64)> = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(SQL_CHUNK) {
            let sql = format!(
                "SELECT id, folder_id, uid FROM messages \
                 WHERE mailbox_id = ?1 AND deleted_at IS NULL AND id IN ({})",
                placeholders(2, chunk.len())
            );
            let mut qy = sqlx::query_as::<_, (i64, i64, i64)>(&sql).bind(mb.0);
            for id in chunk {
                qy = qy.bind(*id);
            }
            for (id, folder, uid) in qy.fetch_all(&mut *tx).await.map_err(map_db_error)? {
                live.insert(id, (folder, uid));
            }
        }
        let p = plan_move(ids, &live, to);
        let mut first_uid = 0i64;
        if !p.to_move.is_empty() {
            let n = p.to_move.len() as i64;
            first_uid = take_uids(&mut tx, mb.0, to.0, n).await?;
            to_uid(first_uid + n - 1)?; // the last uid must still fit
            // 250 messages per statement: each is bound twice (CASE and IN).
            for (c, chunk) in p.to_move.chunks(SQL_CHUNK / 2).enumerate() {
                let base = first_uid + (c * (SQL_CHUNK / 2)) as i64;
                let whens = (0..chunk.len())
                    .map(|k| format!("WHEN ?{} THEN ?{}", 3 + 2 * k, 4 + 2 * k))
                    .collect::<Vec<_>>()
                    .join(" ");
                let ins = (0..chunk.len())
                    .map(|k| format!("?{}", 3 + 2 * k))
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "UPDATE messages SET folder_id = ?1, uid = CASE id {whens} END \
                     WHERE mailbox_id = ?2 AND deleted_at IS NULL AND id IN ({ins})"
                );
                let mut qy = sqlx::query(&sql).bind(to.0).bind(mb.0);
                for (k, id) in chunk.iter().enumerate() {
                    qy = qy.bind(*id).bind(base + k as i64);
                }
                qy.execute(&mut *tx).await.map_err(map_db_error)?;
            }
        }
        let seq = log_message_changes(&mut tx, mb.0, &p.to_move, ChangeOp::Update).await?;
        let moved = moved_refs(p.plan, first_uid)?;
        // Every fallible step is done: nothing can fail after the commit.
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new(
            MoveBatch {
                moved,
                missing: p.missing,
            },
            seq,
        ))
    }

    async fn copy_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        to: FolderId,
    ) -> Result<Committed<CopyBatch>> {
        check_batch_len(ids.len())?;
        check_distinct_ids(ids)?;
        let mut tx = self.begin_locked(mb).await?;
        folder_row(&mut tx, mb.0, to.0).await?;
        let mut live: HashSet<i64> = HashSet::with_capacity(ids.len());
        for chunk in ids.chunks(SQL_CHUNK) {
            let sql = format!(
                "SELECT id FROM messages WHERE mailbox_id = ?1 AND deleted_at IS NULL AND id IN ({})",
                placeholders(2, chunk.len())
            );
            let mut qy = sqlx::query_scalar::<_, i64>(&sql).bind(mb.0);
            for id in chunk {
                qy = qy.bind(*id);
            }
            live.extend(qy.fetch_all(&mut *tx).await.map_err(map_db_error)?);
        }
        let (src, missing) = split_hits(ids, &live);
        if src.is_empty() {
            return Ok(Committed::new(CopyBatch { copied: Vec::new(), missing }, None));
        }
        let n = src.len() as i64;
        let first_uid = take_uids(&mut tx, mb.0, to.0, n).await?;
        to_uid(first_uid + n - 1)?; // the last uid must still fit
        let (first_id, first_seq): (i64, i64) = sqlx::query_as(&r(q::TAKE_MSG_IDS_AND_SEQS))
            .bind(mb.0)
            .bind(n)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_db_error)?
            .ok_or_else(not_found_mailbox)?;
        let mut copied = Vec::with_capacity(src.len());
        for (k, id) in src.iter().enumerate() {
            let k = k as i64;
            let (new_id, uid, seq) = (first_id + k, first_uid + k, first_seq + k);
            sqlx::query(&r(q::COPY_ROW))
                .bind(mb.0)
                .bind(*id)
                .bind(new_id)
                .bind(to.0)
                .bind(uid)
                .execute(&mut *tx)
                .await
                .map_err(map_db_error)?;
            sqlx::query(&r(q::COPY_KEYWORDS))
                .bind(mb.0)
                .bind(*id)
                .bind(new_id)
                .execute(&mut *tx)
                .await
                .map_err(map_db_error)?;
            sqlx::query(&r(q::INSERT_CHANGE))
                .bind(mb.0)
                .bind(seq)
                .bind("message")
                .bind(new_id)
                .bind("create")
                .execute(&mut *tx)
                .await
                .map_err(map_db_error)?;
            copied.push((MessageId(*id), MessageRef { id: MessageId(new_id), uid: to_uid(uid)? }));
        }
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new(CopyBatch { copied, missing }, Some(first_seq + n - 1)))
    }

    async fn expunge(&self, mb: MailboxId, id: MessageId) -> Result<Committed<()>> {
        let mut tx = self.begin_locked(mb).await?;
        let n = sqlx::query(&r(q::EXPUNGE))
            .bind(mb.0)
            .bind(id.0)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
        if n == 0 {
            return Err(not_found_message());
        }
        let seq = log_change(&mut tx, mb.0, ChangeItem::Message(id), ChangeOp::Destroy).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn expunge_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        at: Option<i64>,
    ) -> Result<Committed<BatchOutcome>> {
        check_batch_len(ids.len())?;
        let mut tx = self.begin_locked(mb).await?;
        let mut hit = HashSet::with_capacity(ids.len());
        for chunk in ids.chunks(SQL_CHUNK) {
            // `deleted_at` is the database clock unless a test supplies one.
            let sql = format!(
                "UPDATE messages SET deleted_at = COALESCE(?1, unixepoch()) \
                 WHERE mailbox_id = ?2 AND deleted_at IS NULL AND id IN ({}) RETURNING id",
                placeholders(3, chunk.len())
            );
            let mut qy = sqlx::query_scalar::<_, i64>(&sql).bind(at).bind(mb.0);
            for id in chunk {
                qy = qy.bind(*id);
            }
            hit.extend(qy.fetch_all(&mut *tx).await.map_err(map_db_error)?);
        }
        let (applied, missing) = split_hits(ids, &hit);
        let seq = log_message_changes(&mut tx, mb.0, &applied, ChangeOp::Destroy).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new(
            BatchOutcome {
                applied: applied.len() as u32,
                missing,
            },
            seq,
        ))
    }

    async fn purge_deleted(
        &self,
        mb: MailboxId,
        older_than: i64,
        limit: i64,
    ) -> Result<Committed<u64>> {
        check_purge_limit(limit)?;
        let mut tx = self.begin_locked(mb).await?;
        let n = sqlx::query(
            &crate::repo::sqlite_lit("DELETE FROM messages WHERE rowid IN (\
               SELECT rowid FROM messages \
               WHERE mailbox_id = ?1 AND deleted_at IS NOT NULL AND deleted_at < ?2 \
               AND NOT EXISTS (SELECT 1 FROM deliveries d \
                 WHERE d.mailbox_id = messages.mailbox_id AND d.message_id = messages.id) \
               ORDER BY id LIMIT ?3)"),
        )
        .bind(mb.0)
        .bind(older_than)
        .bind(limit)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?
        .rows_affected();
        tx.commit().await.map_err(map_db_error)?;
        Ok(Committed::new(n, None))
    }

    async fn get_message(&self, mb: MailboxId, id: MessageId) -> Result<MessageDetail> {
        let (mid, uid, size, internal_date, subject, from_addr, flags, thread, folder, hash, deleted): DetailRow =
            sqlx::query_as(
                &crate::repo::sqlite_lit("SELECT id, uid, size, internal_date, subject, from_addr, flags, thread_id, \
                 folder_id, blob_hash, deleted_at FROM messages WHERE mailbox_id = ?1 AND id = ?2"),
            )
            .bind(mb.0)
            .bind(id.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?
            .ok_or_else(not_found_message)?;
        Ok(MessageDetail {
            summary: summary_from_row((mid, uid, size, internal_date, subject, from_addr, flags, thread))?,
            folder: FolderId(folder),
            blob: BlobHash::parse(&hash)?,
            deleted: deleted.is_some(),
        })
    }

    async fn get_raw_ref(&self, mb: MailboxId, id: MessageId) -> Result<RawRef> {
        let (hash, size): (String, i64) = sqlx::query_as(
            &crate::repo::sqlite_lit("SELECT blob_hash, size FROM messages \
             WHERE mailbox_id = ?1 AND id = ?2 AND deleted_at IS NULL"),
        )
        .bind(mb.0)
        .bind(id.0)
        .fetch_optional(self.store.pool())
        .await
        .map_err(map_db_error)?
        .ok_or_else(not_found_message)?;
        Ok(RawRef {
            blob: BlobHash::parse(&hash)?,
            size,
        })
    }

    async fn keywords(&self, mb: MailboxId, id: MessageId) -> Result<Vec<String>> {
        let mut conn = self.store.pool().acquire().await.map_err(map_db_error)?;
        live_message(&mut conn, mb.0, id.0).await?;
        sqlx::query_scalar(&r(q::KEYWORDS_OF))
            .bind(mb.0)
            .bind(id.0)
            .fetch_all(&mut *conn)
            .await
            .map_err(map_db_error)
    }

    async fn keywords_many(&self, mb: MailboxId, ids: &[i64]) -> Result<Vec<(i64, String)>> {
        check_batch_len(ids.len())?;
        check_distinct_ids(ids)?;
        let mut out: Vec<(i64, String)> = Vec::new();
        for chunk in ids.chunks(SQL_CHUNK) {
            let sql = format!(
                "SELECT k.message_id, k.keyword FROM keywords k JOIN messages m \
                 ON m.mailbox_id = k.mailbox_id AND m.id = k.message_id \
                 WHERE k.mailbox_id = ?1 AND m.deleted_at IS NULL AND k.message_id IN ({})",
                placeholders(2, chunk.len())
            );
            let mut qy = sqlx::query_as::<_, (i64, String)>(&sql).bind(mb.0);
            for id in chunk {
                qy = qy.bind(*id);
            }
            out.extend(qy.fetch_all(self.store.pool()).await.map_err(map_db_error)?);
        }
        out.sort();
        Ok(out)
    }

    async fn latest_change_seq(&self, mb: MailboxId) -> Result<i64> {
        sqlx::query_scalar(&r(q::LATEST_SEQ))
            .bind(mb.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?
            .ok_or_else(not_found_mailbox)
    }

    async fn list_folder(
        &self,
        mb: MailboxId,
        folder: FolderId,
        after_uid: u32,
        limit: i64,
    ) -> Result<Vec<MessageSummary>> {
        self.folder_exists(mb, folder).await?;
        let rows: Vec<SummaryRow> = sqlx::query_as(&r(q::LIST_FOLDER))
            .bind(mb.0)
            .bind(folder.0)
            .bind(i64::from(after_uid))
            .bind(limit)
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)?;
        rows.into_iter().map(summary_from_row).collect()
    }

    async fn changes_since(&self, mb: MailboxId, seq: i64, limit: i64) -> Result<ChangePage> {
        changes_since_generic(self.store.pool(), &r(q::CHANGES), mb, seq, limit).await
    }
}
