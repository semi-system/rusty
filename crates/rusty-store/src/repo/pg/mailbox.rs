//! `MailboxRepo` on Postgres (READ COMMITTED).
//!
//! Every write is `tx_retry!(self, Some(mb), ..)`: BEGIN, `SET LOCAL lock_timeout`, then the
//! mailbox row lock `SELECT .. FOR UPDATE` as the first data statement, then the body. The whole
//! unit is re-run on 40P01 / 40001 (bounded) and ends as `Error::Busy` when the retries run out.
//! Blob rows are locked in ascending hash order (`lock_blobs` for inserts,
//! `lock_existing_blobs` for deletes) before any statement fires the refcount trigger. Batches
//! use arrays and `unnest` (about 9 statements for any batch size). No session state: no `SET`
//! (only the transaction-local `set_config(.., true)` in `begin_write`), no temp tables, no
//! advisory locks, no SQL-level `PREPARE`.

use std::collections::{BTreeMap, HashMap, HashSet};

use async_trait::async_trait;
use rusty_core::{AccountId, BlobHash, Error, Flags, FolderId, MailboxId, MessageId, Result};
use sqlx::PgConnection;

use super::tx::{tx_retry, TxResult};
use super::{lock_blobs, lock_existing_blobs, PgRepo};
use crate::mailbox::{
    to_uid, BatchOutcome, ChangeItem, ChangeOp, ChangePage, CopyBatch, DeliverOutcome, MessageRef,
    MessageSummary, MoveBatch, MAX_FOLDER_DEPTH, MAX_KEYWORDS_PER_MESSAGE,
};
use crate::repo::{
    changes_since_generic, check_batch_len, check_distinct_ids, check_purge_limit, depth_verdict, hash_bytes, moved_refs,
    not_found_folder, not_found_mailbox, not_found_message, plan_deliveries, plan_move,
    render_pg as r, split_hits, sql_text as q, summary_from_row, thread_candidates, Committed,
    DeliverEntry, Delivery, FlagUpdate, flag_update, FolderCounts, FolderInfo, JobFence, MailboxRepo, MessageDetail, NewMessage, RawRef,
    counts_from_row, folder_from_row, FolderRow,
    Slot, SummaryRow,
};
use crate::map_db_error;

// ---------------------------------------------------------------- in-transaction primitives

/// Fence and complete the job in the caller's transaction (see `JobQueue::complete_in`). A job
/// that has a domain may only act on a mailbox of that domain (`Invalid` otherwise, review PG5
/// M-c); a stale epoch or lapsed lease is `Conflict`.
async fn complete_job(c: &mut PgConnection, f: JobFence, mailbox: i64) -> TxResult<()> {
    let n = sqlx::query(r(q::COMPLETE_JOB))
        .bind(f.id.0)
        .bind(f.epoch)
        .bind(mailbox)
        .execute(&mut *c)
        .await?
        .rows_affected();
    if n == 1 {
        return Ok(());
    }
    let owned: Option<i64> = sqlx::query_scalar(r(q::JOB_STILL_OWNED))
        .bind(f.id.0)
        .bind(f.epoch)
        .fetch_optional(&mut *c)
        .await?;
    if owned.is_some() {
        Err(Error::Invalid("job belongs to another domain".into()).into())
    } else {
        Err(Error::Conflict("stale job epoch".into()).into())
    }
}

/// Append a change_log row with the next seq of the mailbox; returns that seq.
async fn log_change(
    c: &mut PgConnection,
    mb: i64,
    item: ChangeItem,
    op: ChangeOp,
) -> TxResult<i64> {
    let (kind, item_id) = item.to_row();
    let seq: i64 = sqlx::query_scalar(r(q::TAKE_SEQ))
        .bind(mb)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(not_found_mailbox)?;
    sqlx::query(r(q::INSERT_CHANGE))
        .bind(mb)
        .bind(seq)
        .bind(kind)
        .bind(item_id)
        .bind(op.as_str())
        .execute(&mut *c)
        .await?;
    Ok(seq)
}

/// One `op` change per message id, in order: ONE statement allocates the seq range, ONE inserts
/// the rows. Returns the last seq (`None` for an empty list).
async fn log_message_changes(
    c: &mut PgConnection,
    mb: i64,
    ids: &[i64],
    op: ChangeOp,
) -> TxResult<Option<i64>> {
    if ids.is_empty() {
        return Ok(None);
    }
    let n = ids.len() as i64;
    let first: i64 = sqlx::query_scalar(r(q::TAKE_SEQS))
        .bind(mb)
        .bind(n)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(not_found_mailbox)?;
    let seqs: Vec<i64> = (0..n).map(|i| first + i).collect();
    sqlx::query(
        crate::repo::pg_lit("INSERT INTO change_log (mailbox_id, seq, kind, item_id, op) \
         SELECT $1, t.seq, 'message', t.item, $4 \
         FROM unnest($2::bigint[], $3::bigint[]) AS t(seq, item) ORDER BY t.seq"),
    )
    .bind(mb)
    .bind(&seqs)
    .bind(ids)
    .bind(op.as_str())
    .execute(&mut *c)
    .await?;
    Ok(Some(first + n - 1))
}

async fn folder_row(
    c: &mut PgConnection,
    mb: i64,
    id: i64,
) -> TxResult<(Option<i64>, String)> {
    Ok(sqlx::query_as(r(q::FOLDER_ROW))
        .bind(mb)
        .bind(id)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(not_found_folder)?)
}

/// `Invalid` for system folders (role other than `custom`); returns the parent.
async fn custom_folder(c: &mut PgConnection, mb: i64, id: i64) -> TxResult<Option<i64>> {
    let (parent, role) = folder_row(c, mb, id).await?;
    if role != "custom" {
        return Err(Error::Invalid("system folders cannot be changed".into()).into());
    }
    Ok(parent)
}

async fn ancestor_depth(
    c: &mut PgConnection,
    mb: i64,
    start: i64,
    forbidden: Option<i64>,
) -> TxResult<usize> {
    let rows: Vec<(i64, Option<i64>)> = sqlx::query_as(r(q::ANCESTORS))
        .bind(mb)
        .bind(start)
        .bind(MAX_FOLDER_DEPTH as i64)
        .fetch_all(&mut *c)
        .await?;
    Ok(depth_verdict(&rows, forbidden)?)
}

async fn subtree_height(c: &mut PgConnection, mb: i64, id: i64) -> TxResult<usize> {
    let h: i64 = sqlx::query_scalar(r(q::SUBTREE_HEIGHT))
        .bind(id)
        .bind(mb)
        .bind(MAX_FOLDER_DEPTH as i64)
        .fetch_one(&mut *c)
        .await?;
    usize::try_from(h).map_err(|_| Error::storage("corrupt folder tree").into())
}

async fn live_message(c: &mut PgConnection, mb: i64, id: i64) -> TxResult<(i64, i64)> {
    Ok(sqlx::query_as(r(q::LIVE_MESSAGE))
        .bind(mb)
        .bind(id)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(not_found_message)?)
}

/// Store `msgs` (all in the mailbox `mb`, whose row the caller has locked): blobs locked first
/// (ascending), then ONE statement allocates the id range AND the change-seq range, one per
/// distinct folder allocates the uid range, one thread lookup, one `INSERT .. unnest` per table.
/// `delivery_ids` is empty (no `deliveries` rows) or aligned with `msgs`. `at: Some(t)` is stored
/// verbatim as `internal_date` and `deliveries.created_at`, `None` is the database clock.
/// Returns the refs and the last change seq.
async fn insert_messages(
    c: &mut PgConnection,
    mb: i64,
    msgs: &[&NewMessage],
    delivery_ids: &[&[u8]],
    at: Option<i64>,
    knobs: super::Knobs,
) -> TxResult<(Vec<MessageRef>, i64)> {
    lock_blobs(c, msgs, knobs).await?;
    let n = msgs.len() as i64;

    let (first_id, first_seq): (i64, i64) = sqlx::query_as(r(q::TAKE_MSG_IDS_AND_SEQS))
        .bind(mb)
        .bind(n)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(not_found_mailbox)?;

    // One statement per distinct folder (ascending id) allocates its uid range.
    let mut next_uid: BTreeMap<i64, i64> = BTreeMap::new();
    let mut per_folder: BTreeMap<i64, i64> = BTreeMap::new();
    for m in msgs {
        *per_folder.entry(m.folder.0).or_default() += 1;
    }
    for (folder, count) in &per_folder {
        let first: i64 = sqlx::query_scalar(r(q::TAKE_UIDS))
            .bind(mb)
            .bind(folder)
            .bind(count)
            .fetch_optional(&mut *c)
            .await?
            .ok_or_else(not_found_folder)?;
        next_uid.insert(*folder, first);
    }

    // Threads: one lookup for all candidates, then an in-batch overlay so a reply to an
    // earlier message of the same batch joins its thread (same result as sequential inserts).
    let all: Vec<&str> = msgs.iter().flat_map(|m| thread_candidates(&m.env)).collect();
    let mut known: HashMap<String, i64> = HashMap::new();
    if !all.is_empty() {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            crate::repo::pg_lit("SELECT message_id_hdr, thread_id FROM messages WHERE mailbox_id = $1 AND id IN ( \
               SELECT MIN(id) FROM messages WHERE mailbox_id = $1 \
               AND message_id_hdr = ANY($2) GROUP BY message_id_hdr)"),
        )
        .bind(mb)
        .bind(&all)
        .fetch_all(&mut *c)
        .await?;
        known.extend(rows);
    }

    let mut refs = Vec::with_capacity(msgs.len());
    let (mut ids, mut folders, mut uids, mut hashes, mut sizes) =
        (vec![], vec![], vec![], vec![], vec![]);
    let (mut subjects, mut froms, mut tos, mut hdrs, mut threads, mut flags) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
    for (i, m) in msgs.iter().enumerate() {
        let id = first_id + i as i64;
        let slot = next_uid.get_mut(&m.folder.0).ok_or_else(not_found_folder)?;
        let uid = *slot;
        *slot += 1;
        let thread = thread_candidates(&m.env)
            .iter()
            .find_map(|k| known.get(*k).copied())
            .unwrap_or(id);
        if let Some(h) = &m.env.message_id {
            known.entry(h.clone()).or_insert(thread);
        }
        ids.push(id);
        folders.push(m.folder.0);
        uids.push(uid);
        hashes.push(hash_bytes(&m.hash)?);
        sizes.push(m.size);
        subjects.push(m.env.subject.as_str());
        froms.push(m.env.from.as_str());
        tos.push(m.env.to.as_str());
        hdrs.push(m.env.message_id.as_deref());
        threads.push(thread);
        flags.push(i64::from(m.flags.bits()));
        refs.push(MessageRef {
            id: MessageId(id),
            uid: to_uid(uid)?,
        });
    }
    sqlx::query(
        crate::repo::pg_lit("INSERT INTO messages (mailbox_id, id, folder_id, uid, blob_hash, size, internal_date, \
         subject, from_addr, to_addrs, message_id_hdr, thread_id, flags) \
         SELECT $1, t.id, t.folder_id, t.uid, t.blob_hash, t.size, COALESCE($13::bigint, rs_now()), \
                t.subject, t.from_addr, t.to_addrs, t.hdr, t.thread_id, t.flags \
         FROM unnest($2::bigint[], $3::bigint[], $4::bigint[], $5::bytea[], $6::bigint[], \
                     $7::text[], $8::text[], $9::text[], $10::text[], $11::bigint[], $12::bigint[]) \
              AS t(id, folder_id, uid, blob_hash, size, subject, from_addr, to_addrs, hdr, \
                   thread_id, flags) \
         ORDER BY t.id"),
    )
    .bind(mb)
    .bind(&ids)
    .bind(&folders)
    .bind(&uids)
    .bind(&hashes)
    .bind(&sizes)
    .bind(&subjects)
    .bind(&froms)
    .bind(&tos)
    .bind(&hdrs)
    .bind(&threads)
    .bind(&flags)
    .bind(at)
    .execute(&mut *c)
    .await?;
    if !delivery_ids.is_empty() {
        sqlx::query(
            crate::repo::pg_lit("INSERT INTO deliveries (mailbox_id, delivery_id, message_id, blob_hash, created_at) \
             SELECT $1, t.did, t.mid, t.h, COALESCE($5::bigint, rs_now()) \
             FROM unnest($2::bytea[], $3::bigint[], $4::bytea[]) AS t(did, mid, h) ORDER BY t.mid"),
        )
        .bind(mb)
        .bind(delivery_ids)
        .bind(&ids)
        .bind(&hashes)
        .bind(at)
        .execute(&mut *c)
        .await?;
    }
    let seqs: Vec<i64> = (0..n).map(|i| first_seq + i).collect();
    sqlx::query(
        crate::repo::pg_lit("INSERT INTO change_log (mailbox_id, seq, kind, item_id, op) \
         SELECT $1, t.seq, 'message', t.item, 'create' FROM unnest($2::bigint[], $3::bigint[]) \
         AS t(seq, item) ORDER BY t.seq"),
    )
    .bind(mb)
    .bind(&seqs)
    .bind(&ids)
    .execute(&mut *c)
    .await?;
    Ok((refs, first_seq + n - 1))
}

type DetailRow = (i64, i64, i64, i64, String, String, i64, i64, i64, Vec<u8>, Option<i64>);

#[async_trait]
impl MailboxRepo for PgRepo {
    async fn mailbox_for_account(&self, account: AccountId) -> Result<MailboxId> {
        sqlx::query_scalar::<_, i64>(r(q::MAILBOX_FOR_ACCOUNT))
            .bind(account.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?
            .map(MailboxId)
            .ok_or_else(|| Error::NotFound("mailbox for account".into()))
    }

    async fn folder_exists(&self, mb: MailboxId, folder: FolderId) -> Result<()> {
        sqlx::query_scalar::<_, i64>(r(q::FOLDER_EXISTS))
            .bind(mb.0)
            .bind(folder.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?
            .map(|_| ())
            .ok_or_else(not_found_folder)
    }

    async fn list_folders(&self, mb: MailboxId) -> Result<Vec<FolderInfo>> {
        let rows: Vec<FolderRow> = sqlx::query_as(r(q::LIST_FOLDERS))
            .bind(mb.0)
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)?;
        rows.into_iter().map(folder_from_row).collect()
    }

    async fn folder_counts(&self, mb: MailboxId, folder: FolderId) -> Result<FolderCounts> {
        self.folder_exists(mb, folder).await?;
        let row: (i64, i64) = sqlx::query_as(r(q::FOLDER_COUNTS))
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
        let seq = tx_retry!(self, Some(mb), |c| {
            {
                let n = sqlx::query(r(q::SET_SUBSCRIBED))
                    .bind(mb.0)
                    .bind(id.0)
                    .bind(i64::from(subscribed))
                    .execute(&mut *c)
                    .await?
                    .rows_affected();
                if n == 0 {
                    return Err(not_found_folder().into());
                }
                log_change(c, mb.0, ChangeItem::Folder(id), ChangeOp::Update).await
            }
        })?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn create_folder(
        &self,
        mb: MailboxId,
        parent: Option<FolderId>,
        name: &str,
    ) -> Result<Committed<FolderId>> {
        let (id, seq) = tx_retry!(self, Some(mb), |c| {
            {
                if let Some(p) = parent {
                    let depth = ancestor_depth(c, mb.0, p.0, None).await?;
                    if depth + 1 > MAX_FOLDER_DEPTH {
                        return Err(Error::Invalid("folder tree too deep".into()).into());
                    }
                }
                let uv: i64 = sqlx::query_scalar(
                    crate::repo::pg_lit("UPDATE mailboxes SET uid_validity = GREATEST(rs_now(), uid_validity + 1) \
                     WHERE id = $1 RETURNING uid_validity"),
                )
                .bind(mb.0)
                .fetch_one(&mut *c)
                .await?;
                if uv > i64::from(u32::MAX) {
                    return Err(Error::Quota("folder UIDVALIDITY space exhausted".into()).into());
                }
                let id: i64 = sqlx::query_scalar(r(q::TAKE_FOLDER_ID))
                    .bind(mb.0)
                    .fetch_one(&mut *c)
                    .await?;
                sqlx::query(r(q::INSERT_FOLDER))
                    .bind(mb.0)
                    .bind(id)
                    .bind(parent.map(|p| p.0))
                    .bind(name)
                    .bind(uv)
                    .execute(&mut *c)
                    .await?;
                let seq = log_change(c, mb.0, ChangeItem::Folder(FolderId(id)), ChangeOp::Create)
                    .await?;
                Ok((id, seq))
            }
        })?;
        Ok(Committed::new(FolderId(id), Some(seq)))
    }

    async fn rename_folder(
        &self,
        mb: MailboxId,
        id: FolderId,
        name: &str,
    ) -> Result<Committed<()>> {
        let seq = tx_retry!(self, Some(mb), |c| {
            {
                custom_folder(c, mb.0, id.0).await?;
                sqlx::query(r(q::RENAME_FOLDER))
                    .bind(mb.0)
                    .bind(id.0)
                    .bind(name)
                    .execute(&mut *c)
                    .await?;
                log_change(c, mb.0, ChangeItem::Folder(id), ChangeOp::Update).await
            }
        })?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn move_folder(
        &self,
        mb: MailboxId,
        id: FolderId,
        new_parent: Option<FolderId>,
    ) -> Result<Committed<()>> {
        let seq = tx_retry!(self, Some(mb), |c| {
            {
                let old_parent = custom_folder(c, mb.0, id.0).await?;
                if old_parent == new_parent.map(|p| p.0) {
                    return Ok(None);
                }
                if let Some(p) = new_parent {
                    let depth = ancestor_depth(c, mb.0, p.0, Some(id.0)).await?;
                    let height = subtree_height(c, mb.0, id.0).await?;
                    if depth + height > MAX_FOLDER_DEPTH {
                        return Err(Error::Invalid("folder tree too deep".into()).into());
                    }
                }
                sqlx::query(r(q::REPARENT_FOLDER))
                    .bind(mb.0)
                    .bind(id.0)
                    .bind(new_parent.map(|p| p.0))
                    .execute(&mut *c)
                    .await?;
                Ok(Some(
                    log_change(c, mb.0, ChangeItem::Folder(id), ChangeOp::Update).await?,
                ))
            }
        })?;
        Ok(Committed::new((), seq))
    }

    async fn delete_folder(&self, mb: MailboxId, id: FolderId) -> Result<Committed<()>> {
        let knobs = self.knobs;
        let seq = tx_retry!(self, Some(mb), |c| {
            {
                custom_folder(c, mb.0, id.0).await?;
                let children: i64 = sqlx::query_scalar(r(q::CHILD_COUNT))
                    .bind(mb.0)
                    .bind(id.0)
                    .fetch_one(&mut *c)
                    .await?;
                if children > 0 {
                    return Err(Error::Conflict("folder has subfolders".into()).into());
                }
                let live: i64 = sqlx::query_scalar(r(q::LIVE_COUNT))
                    .bind(mb.0)
                    .bind(id.0)
                    .fetch_one(&mut *c)
                    .await?;
                if live > 0 {
                    return Err(Error::Conflict("folder still contains messages".into()).into());
                }
                // The deletes below fire the refcount trigger on these blobs: lock them first,
                // in ascending hash order (message order is NOT hash order).
                let hashes: Vec<Vec<u8>> = sqlx::query_scalar(
                    crate::repo::pg_lit("SELECT blob_hash FROM messages \
                     WHERE mailbox_id = $1 AND folder_id = $2 AND deleted_at IS NOT NULL \
                     ORDER BY id"),
                )
                .bind(mb.0)
                .bind(id.0)
                .fetch_all(&mut *c)
                .await?;
                lock_existing_blobs(c, &hashes, knobs).await?;
                sqlx::query(r(q::DELETE_FOLDER_MESSAGES))
                    .bind(mb.0)
                    .bind(id.0)
                    .execute(&mut *c)
                    .await?;
                sqlx::query(r(q::DELETE_FOLDER))
                    .bind(mb.0)
                    .bind(id.0)
                    .execute(&mut *c)
                    .await?;
                log_change(c, mb.0, ChangeItem::Folder(id), ChangeOp::Destroy).await
            }
        })?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn append(
        &self,
        mb: MailboxId,
        msg: &NewMessage,
        delivery: Option<&Delivery>,
        fence: Option<JobFence>,
    ) -> Result<Committed<DeliverOutcome>> {
        let knobs = self.knobs;
        let (outcome, seq) = tx_retry!(self, Some(mb), |c| {
            {
                let mut dup = None;
                if let Some(d) = delivery {
                    // The mailbox row is locked: no concurrent delivery of this mailbox can
                    // slip in between this lookup and the insert below. READ COMMITTED gives
                    // this statement a fresh snapshot after the lock wait.
                    let hit: Option<(i64, Vec<u8>)> = sqlx::query_as(r(q::DELIVERY_LOOKUP))
                        .bind(mb.0)
                        .bind(&d.id.0[..])
                        .fetch_optional(&mut *c)
                        .await?;
                    if let Some((msg_id, blob_hash)) = hit {
                        if blob_hash != hash_bytes(&msg.hash)? {
                            return Err(Error::Conflict(
                                "delivery id reused with different content".into(),
                            )
                            .into());
                        }
                        let uid: i64 = sqlx::query_scalar(r(q::MESSAGE_UID))
                            .bind(mb.0)
                            .bind(msg_id)
                            .fetch_optional(&mut *c)
                            .await?
                            .ok_or_else(|| Error::storage("delivery refers to a missing message"))?;
                        dup = Some(MessageRef {
                            id: MessageId(msg_id),
                            uid: to_uid(uid)?,
                        });
                    }
                }
                let (outcome, seq) = match dup {
                    Some(r) => (DeliverOutcome::Duplicate(r), None),
                    None => {
                        let dids: Vec<&[u8]> = delivery.iter().map(|d| &d.id.0[..]).collect();
                        let at = delivery.and_then(|d| d.created_at).or(msg.internal_date);
                        let (mut refs, seq) =
                            insert_messages(c, mb.0, &[msg], &dids, at, knobs).await?;
                        (DeliverOutcome::Created(refs.remove(0)), Some(seq))
                    }
                };
                if let Some(f) = fence {
                    complete_job(c, f, mb.0).await?;
                }
                Ok((outcome, seq))
            }
        })?;
        Ok(Committed::new(outcome, seq))
    }

    async fn append_many(
        &self,
        mb: MailboxId,
        msgs: &[NewMessage],
    ) -> Result<Committed<Vec<MessageRef>>> {
        check_batch_len(msgs.len())?;
        let knobs = self.knobs;
        let (refs, seq) = tx_retry!(self, Some(mb), |c| {
            {
                let refs: Vec<&NewMessage> = msgs.iter().collect();
                insert_messages(c, mb.0, &refs, &[], None, knobs).await
            }
        })?;
        Ok(Committed::new(refs, Some(seq)))
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
        let knobs = self.knobs;
        let (outcomes, seq) = tx_retry!(self, Some(mb), |c| {
            {
                // Deliveries that already exist: ONE lookup for all distinct ids.
                let mut seen = HashSet::new();
                let wanted: Vec<&[u8]> = items
                    .iter()
                    .filter(|i| seen.insert(i.delivery_id.0))
                    .map(|i| &i.delivery_id.0[..])
                    .collect();
                let rows: Vec<(Vec<u8>, i64, Vec<u8>)> = sqlx::query_as(
                    crate::repo::pg_lit("SELECT delivery_id, message_id, blob_hash FROM deliveries \
                     WHERE mailbox_id = $1 AND delivery_id = ANY($2)"),
                )
                .bind(mb.0)
                .bind(&wanted)
                .fetch_all(&mut *c)
                .await?;
                let existing: HashMap<Vec<u8>, (i64, String)> = rows
                    .into_iter()
                    .map(|(d, m, h)| (d, (m, hex::encode(h))))
                    .collect();
                let plan = plan_deliveries(items, &existing)?;
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
                let old_uid: HashMap<i64, i64> = if old_ids.is_empty() {
                    HashMap::new()
                } else {
                    sqlx::query_as::<_, (i64, i64)>(
                        crate::repo::pg_lit("SELECT id, uid FROM messages WHERE mailbox_id = $1 AND id = ANY($2)"),
                    )
                    .bind(mb.0)
                    .bind(&old_ids)
                    .fetch_all(&mut *c)
                    .await?
                    .into_iter()
                    .collect()
                };
                let (refs, seq) = if plan.creates.is_empty() {
                    (Vec::new(), None)
                } else {
                    let msgs: Vec<&NewMessage> =
                        plan.creates.iter().map(|&i| &items[i].msg).collect();
                    let dids: Vec<&[u8]> = plan
                        .creates
                        .iter()
                        .map(|&i| &items[i].delivery_id.0[..])
                        .collect();
                    let (refs, seq) = insert_messages(c, mb.0, &msgs, &dids, at, knobs).await?;
                    (refs, Some(seq))
                };
                if let Some(f) = fence {
                    complete_job(c, f, mb.0).await?;
                }
                let outcomes = plan
                    .slots
                    .iter()
                    .map(|s| match *s {
                        Slot::Created(n) => Ok(DeliverOutcome::Created(refs[n])),
                        Slot::DuplicateOfNew(n) => Ok(DeliverOutcome::Duplicate(refs[n])),
                        Slot::Existing(m) => {
                            let uid = old_uid.get(&m).copied().ok_or_else(|| {
                                Error::storage("delivery refers to a missing message")
                            })?;
                            Ok(DeliverOutcome::Duplicate(MessageRef {
                                id: MessageId(m),
                                uid: to_uid(uid)?,
                            }))
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok((outcomes, seq))
            }
        })?;
        Ok(Committed::new(outcomes, seq))
    }

    async fn set_flags(&self, mb: MailboxId, id: MessageId, flags: Flags) -> Result<Committed<()>> {
        let seq = tx_retry!(self, Some(mb), |c| {
            {
                let n = sqlx::query(r(q::SET_FLAGS))
                    .bind(mb.0)
                    .bind(id.0)
                    .bind(i64::from(flags.bits()))
                    .execute(&mut *c)
                    .await?
                    .rows_affected();
                if n == 0 {
                    return Err(not_found_message().into());
                }
                log_change(c, mb.0, ChangeItem::Message(id), ChangeOp::Update).await
            }
        })?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn set_flags_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        flags: Flags,
    ) -> Result<Committed<BatchOutcome>> {
        check_batch_len(ids.len())?;
        let (out, seq) = tx_retry!(self, Some(mb), |c| {
            {
                let hit: HashSet<i64> = sqlx::query_scalar::<_, i64>(
                    crate::repo::pg_lit("UPDATE messages SET flags = $3 WHERE mailbox_id = $1 \
                     AND deleted_at IS NULL AND id = ANY($2) RETURNING id"),
                )
                .bind(mb.0)
                .bind(ids)
                .bind(i64::from(flags.bits()))
                .fetch_all(&mut *c)
                .await?
                .into_iter()
                .collect();
                let (applied, missing) = split_hits(ids, &hit);
                let seq = log_message_changes(c, mb.0, &applied, ChangeOp::Update).await?;
                Ok((
                    BatchOutcome {
                        applied: applied.len() as u32,
                        missing,
                    },
                    seq,
                ))
            }
        })?;
        Ok(Committed::new(out, seq))
    }

    async fn update_flags_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        add: Flags,
        remove: Flags,
    ) -> Result<Committed<FlagUpdate>> {
        check_batch_len(ids.len())?;
        check_distinct_ids(ids)?;
        let (out, seq) = tx_retry!(self, Some(mb), |c| {
            {
                let now: HashMap<i64, i64> = sqlx::query_as::<_, (i64, i64)>(
                    crate::repo::pg_lit("UPDATE messages SET flags = (flags | $3) & ~$4 WHERE mailbox_id = $1 \
                     AND deleted_at IS NULL AND id = ANY($2) RETURNING id, flags"),
                )
                .bind(mb.0)
                .bind(ids)
                .bind(i64::from(add.bits()))
                .bind(i64::from(remove.bits()))
                .fetch_all(&mut *c)
                .await?
                .into_iter()
                .collect();
                let hit: HashSet<i64> = now.keys().copied().collect();
                let (applied, missing) = split_hits(ids, &hit);
                let seq = log_message_changes(c, mb.0, &applied, ChangeOp::Update).await?;
                Ok((flag_update(&applied, &now, missing)?, seq))
            }
        })?;
        Ok(Committed::new(out, seq))
    }

    async fn expunge_deleted_many(&self, mb: MailboxId, ids: &[i64]) -> Result<Committed<BatchOutcome>> {
        check_batch_len(ids.len())?;
        check_distinct_ids(ids)?;
        let (out, seq) = tx_retry!(self, Some(mb), |c| {
            {
                let hit: HashSet<i64> = sqlx::query_scalar::<_, i64>(
                    crate::repo::pg_lit("UPDATE messages SET deleted_at = rs_now() \
                     WHERE mailbox_id = $1 AND deleted_at IS NULL AND (flags & 16) <> 0 AND id = ANY($2) RETURNING id"),
                )
                .bind(mb.0)
                .bind(ids)
                .fetch_all(&mut *c)
                .await?
                .into_iter()
                .collect();
                let (applied, missing) = split_hits(ids, &hit);
                let seq = log_message_changes(c, mb.0, &applied, ChangeOp::Destroy).await?;
                Ok((BatchOutcome { applied: applied.len() as u32, missing }, seq))
            }
        })?;
        Ok(Committed::new(out, seq))
    }

    async fn add_keyword(
        &self,
        mb: MailboxId,
        id: MessageId,
        keyword: &str,
    ) -> Result<Committed<()>> {
        let seq = tx_retry!(self, Some(mb), |c| {
            {
                live_message(c, mb.0, id.0).await?;
                let present: Option<i64> = sqlx::query_scalar(r(q::KEYWORD_PRESENT))
                    .bind(mb.0)
                    .bind(id.0)
                    .bind(keyword)
                    .fetch_optional(&mut *c)
                    .await?;
                if present.is_some() {
                    return Ok(None);
                }
                let count: i64 = sqlx::query_scalar(r(q::KEYWORD_COUNT))
                    .bind(mb.0)
                    .bind(id.0)
                    .fetch_one(&mut *c)
                    .await?;
                if usize::try_from(count).map_or(true, |n| n >= MAX_KEYWORDS_PER_MESSAGE) {
                    return Err(Error::Quota("too many keywords on message".into()).into());
                }
                sqlx::query(r(q::KEYWORD_INSERT))
                    .bind(mb.0)
                    .bind(id.0)
                    .bind(keyword)
                    .execute(&mut *c)
                    .await?;
                Ok(Some(
                    log_change(c, mb.0, ChangeItem::Message(id), ChangeOp::Update).await?,
                ))
            }
        })?;
        Ok(Committed::new((), seq))
    }

    async fn remove_keyword(
        &self,
        mb: MailboxId,
        id: MessageId,
        keyword: &str,
    ) -> Result<Committed<()>> {
        let seq = tx_retry!(self, Some(mb), |c| {
            {
                live_message(c, mb.0, id.0).await?;
                let n = sqlx::query(r(q::KEYWORD_DELETE))
                    .bind(mb.0)
                    .bind(id.0)
                    .bind(keyword)
                    .execute(&mut *c)
                    .await?
                    .rows_affected();
                if n == 0 {
                    return Ok(None);
                }
                Ok(Some(
                    log_change(c, mb.0, ChangeItem::Message(id), ChangeOp::Update).await?,
                ))
            }
        })?;
        Ok(Committed::new((), seq))
    }

    async fn move_message(
        &self,
        mb: MailboxId,
        id: MessageId,
        to: FolderId,
    ) -> Result<Committed<MessageRef>> {
        let (mref, seq) = tx_retry!(self, Some(mb), |c| {
            {
                let (folder, uid) = live_message(c, mb.0, id.0).await?;
                if folder == to.0 {
                    return Ok((
                        MessageRef {
                            id,
                            uid: to_uid(uid)?,
                        },
                        None,
                    ));
                }
                let first: i64 = sqlx::query_scalar(r(q::TAKE_UIDS))
                    .bind(mb.0)
                    .bind(to.0)
                    .bind(1i64)
                    .fetch_optional(&mut *c)
                    .await?
                    .ok_or_else(not_found_folder)?;
                let new_uid = to_uid(first)?;
                sqlx::query(r(q::MOVE_MESSAGE))
                    .bind(mb.0)
                    .bind(id.0)
                    .bind(to.0)
                    .bind(i64::from(new_uid))
                    .execute(&mut *c)
                    .await?;
                let seq = log_change(c, mb.0, ChangeItem::Message(id), ChangeOp::Update).await?;
                Ok((MessageRef { id, uid: new_uid }, Some(seq)))
            }
        })?;
        Ok(Committed::new(mref, seq))
    }

    async fn move_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        to: FolderId,
    ) -> Result<Committed<MoveBatch>> {
        check_batch_len(ids.len())?;
        check_distinct_ids(ids)?;
        let (batch, seq) = tx_retry!(self, Some(mb), |c| {
            {
                folder_row(c, mb.0, to.0).await?;
                let live: HashMap<i64, (i64, i64)> =
                    sqlx::query_as::<_, (i64, i64, i64)>(
                        crate::repo::pg_lit("SELECT id, folder_id, uid FROM messages \
                         WHERE mailbox_id = $1 AND deleted_at IS NULL AND id = ANY($2)"),
                    )
                    .bind(mb.0)
                    .bind(ids)
                    .fetch_all(&mut *c)
                    .await?
                    .into_iter()
                    .map(|(id, folder, uid)| (id, (folder, uid)))
                    .collect();
                let p = plan_move(ids, &live, to);
                let mut first_uid = 0i64;
                if !p.to_move.is_empty() {
                    let n = p.to_move.len() as i64;
                    first_uid = sqlx::query_scalar(r(q::TAKE_UIDS))
                        .bind(mb.0)
                        .bind(to.0)
                        .bind(n)
                        .fetch_optional(&mut *c)
                        .await?
                        .ok_or_else(not_found_folder)?;
                    to_uid(first_uid + n - 1)?; // the last uid must still fit
                    let uids: Vec<i64> = (0..n).map(|k| first_uid + k).collect();
                    sqlx::query(
                        crate::repo::pg_lit("UPDATE messages m SET folder_id = $2, uid = t.uid \
                         FROM unnest($3::bigint[], $4::bigint[]) AS t(id, uid) \
                         WHERE m.mailbox_id = $1 AND m.id = t.id AND m.deleted_at IS NULL"),
                    )
                    .bind(mb.0)
                    .bind(to.0)
                    .bind(&p.to_move)
                    .bind(&uids)
                    .execute(&mut *c)
                    .await?;
                }
                let seq = log_message_changes(c, mb.0, &p.to_move, ChangeOp::Update).await?;
                let moved = moved_refs(p.plan, first_uid)?;
                Ok((
                    MoveBatch {
                        moved,
                        missing: p.missing,
                    },
                    seq,
                ))
            }
        })?;
        Ok(Committed::new(batch, seq))
    }

    async fn copy_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        to: FolderId,
    ) -> Result<Committed<CopyBatch>> {
        check_batch_len(ids.len())?;
        check_distinct_ids(ids)?;
        let knobs = self.knobs;
        let (batch, seq) = tx_retry!(self, Some(mb), |c| {
            {
                folder_row(c, mb.0, to.0).await?;
                let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
                    crate::repo::pg_lit("SELECT id, blob_hash FROM messages \
                     WHERE mailbox_id = $1 AND deleted_at IS NULL AND id = ANY($2)"),
                )
                .bind(mb.0)
                .bind(ids)
                .fetch_all(&mut *c)
                .await?;
                let live: HashMap<i64, Vec<u8>> = rows.into_iter().collect();
                let hit: HashSet<i64> = live.keys().copied().collect();
                let (src, missing) = split_hits(ids, &hit);
                if src.is_empty() {
                    return Ok((CopyBatch { copied: Vec::new(), missing }, None));
                }
                // Blob rows are locked in ascending hash order before the refcount trigger fires.
                let hashes: Vec<Vec<u8>> = src.iter().filter_map(|id| live.get(id).cloned()).collect();
                lock_existing_blobs(c, &hashes, knobs).await?;
                let n = src.len() as i64;
                let first_uid: i64 = sqlx::query_scalar(r(q::TAKE_UIDS))
                    .bind(mb.0)
                    .bind(to.0)
                    .bind(n)
                    .fetch_optional(&mut *c)
                    .await?
                    .ok_or_else(not_found_folder)?;
                to_uid(first_uid + n - 1)?; // the last uid must still fit
                let (first_id, first_seq): (i64, i64) = sqlx::query_as(r(q::TAKE_MSG_IDS_AND_SEQS))
                    .bind(mb.0)
                    .bind(n)
                    .fetch_optional(&mut *c)
                    .await?
                    .ok_or_else(not_found_mailbox)?;
                let new_ids: Vec<i64> = (0..n).map(|k| first_id + k).collect();
                let uids: Vec<i64> = (0..n).map(|k| first_uid + k).collect();
                let seqs: Vec<i64> = (0..n).map(|k| first_seq + k).collect();
                sqlx::query(
                    crate::repo::pg_lit("INSERT INTO messages (mailbox_id, id, folder_id, uid, blob_hash, size, \
                     internal_date, subject, from_addr, to_addrs, message_id_hdr, thread_id, flags) \
                     SELECT m.mailbox_id, t.new_id, $2, t.uid, m.blob_hash, m.size, m.internal_date, \
                            m.subject, m.from_addr, m.to_addrs, m.message_id_hdr, m.thread_id, m.flags \
                     FROM unnest($3::bigint[], $4::bigint[], $5::bigint[]) AS t(src, new_id, uid) \
                     JOIN messages m ON m.mailbox_id = $1 AND m.id = t.src AND m.deleted_at IS NULL \
                     ORDER BY t.new_id"),
                )
                .bind(mb.0)
                .bind(to.0)
                .bind(&src)
                .bind(&new_ids)
                .bind(&uids)
                .execute(&mut *c)
                .await?;
                sqlx::query(
                    crate::repo::pg_lit("INSERT INTO keywords (mailbox_id, message_id, keyword) \
                     SELECT k.mailbox_id, t.new_id, k.keyword \
                     FROM unnest($2::bigint[], $3::bigint[]) AS t(src, new_id) \
                     JOIN keywords k ON k.mailbox_id = $1 AND k.message_id = t.src \
                     ORDER BY t.new_id, k.keyword"),
                )
                .bind(mb.0)
                .bind(&src)
                .bind(&new_ids)
                .execute(&mut *c)
                .await?;
                sqlx::query(
                    crate::repo::pg_lit("INSERT INTO change_log (mailbox_id, seq, kind, item_id, op) \
                     SELECT $1, t.seq, 'message', t.item, 'create' \
                     FROM unnest($2::bigint[], $3::bigint[]) AS t(seq, item) ORDER BY t.seq"),
                )
                .bind(mb.0)
                .bind(&seqs)
                .bind(&new_ids)
                .execute(&mut *c)
                .await?;
                let mut copied = Vec::with_capacity(src.len());
                for (k, id) in src.iter().enumerate() {
                    copied.push((
                        MessageId(*id),
                        MessageRef { id: MessageId(first_id + k as i64), uid: to_uid(first_uid + k as i64)? },
                    ));
                }
                Ok((CopyBatch { copied, missing }, Some(first_seq + n - 1)))
            }
        })?;
        Ok(Committed::new(batch, seq))
    }

    async fn expunge(&self, mb: MailboxId, id: MessageId) -> Result<Committed<()>> {
        let seq = tx_retry!(self, Some(mb), |c| {
            {
                let n = sqlx::query(r(q::EXPUNGE))
                    .bind(mb.0)
                    .bind(id.0)
                    .execute(&mut *c)
                    .await?
                    .rows_affected();
                if n == 0 {
                    return Err(not_found_message().into());
                }
                log_change(c, mb.0, ChangeItem::Message(id), ChangeOp::Destroy).await
            }
        })?;
        Ok(Committed::new((), Some(seq)))
    }

    async fn expunge_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        at: Option<i64>,
    ) -> Result<Committed<BatchOutcome>> {
        check_batch_len(ids.len())?;
        let (out, seq) = tx_retry!(self, Some(mb), |c| {
            {
                let hit: HashSet<i64> = sqlx::query_scalar::<_, i64>(
                    crate::repo::pg_lit("UPDATE messages SET deleted_at = COALESCE($3::bigint, rs_now()) \
                     WHERE mailbox_id = $1 AND deleted_at IS NULL AND id = ANY($2) RETURNING id"),
                )
                .bind(mb.0)
                .bind(ids)
                .bind(at)
                .fetch_all(&mut *c)
                .await?
                .into_iter()
                .collect();
                let (applied, missing) = split_hits(ids, &hit);
                let seq = log_message_changes(c, mb.0, &applied, ChangeOp::Destroy).await?;
                Ok((
                    BatchOutcome {
                        applied: applied.len() as u32,
                        missing,
                    },
                    seq,
                ))
            }
        })?;
        Ok(Committed::new(out, seq))
    }

    async fn purge_deleted(
        &self,
        mb: MailboxId,
        older_than: i64,
        limit: i64,
    ) -> Result<Committed<u64>> {
        check_purge_limit(limit)?;
        let knobs = self.knobs;
        let n = tx_retry!(self, Some(mb), |c| {
            {
                let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
                    crate::repo::pg_lit("SELECT id, blob_hash FROM messages \
                     WHERE mailbox_id = $1 AND deleted_at IS NOT NULL AND deleted_at < $2 \
                     AND NOT EXISTS (SELECT 1 FROM deliveries d \
                       WHERE d.mailbox_id = messages.mailbox_id AND d.message_id = messages.id) \
                     ORDER BY id LIMIT $3"),
                )
                .bind(mb.0)
                .bind(older_than)
                .bind(limit)
                .fetch_all(&mut *c)
                .await?;
                if rows.is_empty() {
                    return Ok(0u64);
                }
                let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
                let hashes: Vec<Vec<u8>> = rows.into_iter().map(|(_, h)| h).collect();
                // The delete fires the refcount trigger: lock the blobs first, ascending.
                lock_existing_blobs(c, &hashes, knobs).await?;
                let n = sqlx::query(
                    crate::repo::pg_lit("DELETE FROM messages WHERE mailbox_id = $1 AND id = ANY($2)"),
                )
                .bind(mb.0)
                .bind(&ids)
                .execute(&mut *c)
                .await?
                .rows_affected();
                Ok(n)
            }
        })?;
        Ok(Committed::new(n, None))
    }

    async fn get_message(&self, mb: MailboxId, id: MessageId) -> Result<MessageDetail> {
        let (mid, uid, size, internal_date, subject, from_addr, flags, thread, folder, hash, deleted): DetailRow =
            sqlx::query_as(
                crate::repo::pg_lit("SELECT id, uid, size, internal_date, subject, from_addr, flags, thread_id, \
                 folder_id, blob_hash, deleted_at FROM messages WHERE mailbox_id = $1 AND id = $2"),
            )
            .bind(mb.0)
            .bind(id.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?
            .ok_or_else(not_found_message)?;
        Ok(MessageDetail {
            summary: summary_from_row((
                mid,
                uid,
                size,
                internal_date,
                subject,
                from_addr,
                flags,
                thread,
            ))?,
            folder: FolderId(folder),
            blob: BlobHash::parse(&hex::encode(hash))?,
            deleted: deleted.is_some(),
        })
    }

    async fn get_raw_ref(&self, mb: MailboxId, id: MessageId) -> Result<RawRef> {
        let (hash, size): (Vec<u8>, i64) = sqlx::query_as(
            crate::repo::pg_lit("SELECT blob_hash, size FROM messages \
             WHERE mailbox_id = $1 AND id = $2 AND deleted_at IS NULL"),
        )
        .bind(mb.0)
        .bind(id.0)
        .fetch_optional(self.store.pool())
        .await
        .map_err(map_db_error)?
        .ok_or_else(not_found_message)?;
        Ok(RawRef {
            blob: BlobHash::parse(&hex::encode(hash))?,
            size,
        })
    }

    async fn keywords(&self, mb: MailboxId, id: MessageId) -> Result<Vec<String>> {
        let live = sqlx::query_as::<_, (i64, i64)>(r(q::LIVE_MESSAGE))
            .bind(mb.0)
            .bind(id.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?;
        if live.is_none() {
            return Err(not_found_message());
        }
        sqlx::query_scalar(r(q::KEYWORDS_OF))
            .bind(mb.0)
            .bind(id.0)
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)
    }

    async fn keywords_many(&self, mb: MailboxId, ids: &[i64]) -> Result<Vec<(i64, String)>> {
        check_batch_len(ids.len())?;
        check_distinct_ids(ids)?;
        sqlx::query_as::<_, (i64, String)>(
            crate::repo::pg_lit("SELECT k.message_id, k.keyword FROM keywords k JOIN messages m \
             ON m.mailbox_id = k.mailbox_id AND m.id = k.message_id \
             WHERE k.mailbox_id = $1 AND m.deleted_at IS NULL AND k.message_id = ANY($2) \
             ORDER BY k.message_id, k.keyword COLLATE \"C\""),
        )
        .bind(mb.0)
        .bind(ids)
        .fetch_all(self.store.pool())
        .await
        .map_err(map_db_error)
    }

    async fn latest_change_seq(&self, mb: MailboxId) -> Result<i64> {
        sqlx::query_scalar(r(q::LATEST_SEQ))
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
        let rows: Vec<SummaryRow> = sqlx::query_as(r(q::LIST_FOLDER))
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
        changes_since_generic(self.store.pool(), r(q::CHANGES), mb, seq, limit).await
    }
}
