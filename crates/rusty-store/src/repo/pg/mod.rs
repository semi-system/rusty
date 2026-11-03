//! Postgres implementation of the repositories (directory: `directory.rs`; mailbox: spike,
//! PG4 replaces it). Transaction helper, retry loop and test knobs: `tx.rs`.

mod directory;
pub mod tx;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rusty_core::{AccountId, Error, FolderId, MailboxId, MessageId, Result, ThreadId};
use sqlx::PgConnection;

use super::{
    depth_verdict, hash_bytes, not_found_folder, not_found_mailbox, render_pg as r,
    sql_text as q, thread_candidates, Delivery, JobFence, MailboxRepo, NewMessage,
};
use crate::mailbox::{
    to_flags, to_uid, ChangeItem, ChangeOp, ChangePage, DeliverOutcome, MessageRef,
    MessageSummary, MAX_FOLDER_DEPTH,
};
use crate::{map_db_error, PgStore};
use tx::{tx_retry, TxResult};

pub use tx::{is_retryable, Knobs, PgRetryStats, MAX_ATTEMPTS};

#[derive(Clone)]
pub struct PgRepo {
    store: PgStore,
    stats: Arc<PgRetryStats>,
    knobs: Knobs,
    /// Test knob: number of upcoming attempts that raise a real 40P01.
    inject_deadlocks: Arc<AtomicU32>,
}

impl PgRepo {
    pub fn new(store: PgStore) -> Self {
        Self {
            store,
            stats: Arc::default(),
            knobs: Knobs::default(),
            inject_deadlocks: Arc::default(),
        }
    }

    #[doc(hidden)]
    pub fn with_knobs_for_test(mut self, knobs: Knobs) -> Self {
        self.knobs = knobs;
        self
    }

    /// Test knob: the next `n` transaction attempts of `tx_retry!` operations fail with a real
    /// 40P01 raised by the server.
    #[doc(hidden)]
    pub fn inject_deadlocks_for_test(&self, n: u32) {
        self.inject_deadlocks.store(n, Ordering::SeqCst);
    }

    pub fn stats(&self) -> &PgRetryStats {
        &self.stats
    }
}

/// Insert missing blob rows and lock ALL of them (`FOR NO KEY UPDATE`) in ascending hash order.
///
/// After this call the refcount trigger of any message insert finds the rows already locked by
/// this transaction, so no further `blobs` lock is ever acquired out of order. Loops (bounded)
/// if a concurrent GC deleted an unreferenced row between the insert and the lock.
async fn lock_blobs(
    c: &mut PgConnection,
    msgs: &[NewMessage],
    knobs: Knobs,
) -> TxResult<()> {
    // BTreeMap over raw bytes: ascending byte order == bytea order in Postgres.
    let mut blobs: BTreeMap<Vec<u8>, i64> = BTreeMap::new();
    for m in msgs {
        blobs.entry(hash_bytes(&m.hash)?).or_insert(m.size);
    }
    if knobs.unordered_blob_locks {
        // Fault-injection path: input order, one lock per statement, optional pause.
        let mut first = true;
        for m in msgs {
            let h = hash_bytes(&m.hash)?;
            sqlx::query("INSERT INTO blobs (hash, size) VALUES ($1, $2) ON CONFLICT (hash) DO NOTHING")
                .bind(&h)
                .bind(m.size)
                .execute(&mut *c)
                .await?;
            sqlx::query("SELECT hash FROM blobs WHERE hash = $1 FOR NO KEY UPDATE")
                .bind(&h)
                .fetch_all(&mut *c)
                .await?;
            if first && knobs.hold_after_first_blob_ms > 0 {
                sqlx::query("SELECT pg_sleep($1)")
                    .bind(f64::from(knobs.hold_after_first_blob_ms) / 1000.0)
                    .execute(&mut *c)
                    .await?;
            }
            first = false;
        }
        return Ok(());
    }
    let hashes: Vec<Vec<u8>> = blobs.keys().cloned().collect();
    let sizes: Vec<i64> = blobs.values().copied().collect();
    for _ in 0..3 {
        sqlx::query(
            "INSERT INTO blobs (hash, size) \
             SELECT t.h, t.s FROM unnest($1::bytea[], $2::bigint[]) AS t(h, s) ORDER BY t.h \
             ON CONFLICT (hash) DO NOTHING",
        )
        .bind(&hashes)
        .bind(&sizes)
        .execute(&mut *c)
        .await?;
        let locked: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT hash FROM blobs WHERE hash = ANY($1) ORDER BY hash FOR NO KEY UPDATE",
        )
        .bind(&hashes)
        .fetch_all(&mut *c)
        .await?;
        if locked.len() == hashes.len() {
            return Ok(());
        }
    }
    Err(Error::storage("blob row disappeared repeatedly").into())
}

/// Store `msgs` (all in the mailbox `mb`, whose row the caller has locked).
async fn insert_messages(
    c: &mut PgConnection,
    mb: i64,
    msgs: &[NewMessage],
    delivery: Option<&Delivery>,
    knobs: Knobs,
) -> TxResult<Vec<MessageRef>> {
    lock_blobs(c, msgs, knobs).await?;
    let n = msgs.len() as i64;

    // One statement allocates the id range AND the change-seq range.
    let (first_id, first_seq): (i64, i64) = sqlx::query_as(&r(q::TAKE_MSG_IDS_AND_SEQS))
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
        let first: i64 = sqlx::query_scalar(&r(q::TAKE_UIDS))
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
    let mut known: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    if !all.is_empty() {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT message_id_hdr, thread_id FROM messages WHERE mailbox_id = $1 AND id IN ( \
               SELECT MIN(id) FROM messages WHERE mailbox_id = $1 \
               AND message_id_hdr = ANY($2) GROUP BY message_id_hdr)",
        )
        .bind(mb)
        .bind(&all)
        .fetch_all(&mut *c)
        .await?;
        known.extend(rows);
    }

    let mut refs = Vec::with_capacity(msgs.len());
    let (mut ids, mut folders, mut uids, mut hashes, mut sizes) = (vec![], vec![], vec![], vec![], vec![]);
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
        "INSERT INTO messages (mailbox_id, id, folder_id, uid, blob_hash, size, internal_date, \
         subject, from_addr, to_addrs, message_id_hdr, thread_id, flags) \
         SELECT $1, t.id, t.folder_id, t.uid, t.blob_hash, t.size, rs_now(), t.subject, \
                t.from_addr, t.to_addrs, t.hdr, t.thread_id, t.flags \
         FROM unnest($2::bigint[], $3::bigint[], $4::bigint[], $5::bytea[], $6::bigint[], \
                     $7::text[], $8::text[], $9::text[], $10::text[], $11::bigint[], $12::bigint[]) \
              AS t(id, folder_id, uid, blob_hash, size, subject, from_addr, to_addrs, hdr, \
                   thread_id, flags) \
         ORDER BY t.id",
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
    .execute(&mut *c)
    .await?;
    if let Some(d) = delivery {
        sqlx::query(
            "INSERT INTO deliveries (mailbox_id, delivery_id, message_id, blob_hash, created_at) \
             VALUES ($1, $2, $3, $4, COALESCE($5, rs_now()))",
        )
        .bind(mb)
        .bind(&d.id.0[..])
        .bind(first_id)
        .bind(hash_bytes(&msgs[0].hash)?)
        .bind(d.created_at)
        .execute(&mut *c)
        .await?;
    }
    let seqs: Vec<i64> = (0..n).map(|i| first_seq + i).collect();
    sqlx::query(
        "INSERT INTO change_log (mailbox_id, seq, kind, item_id, op) \
         SELECT $1, t.seq, 'message', t.item, 'create' FROM unnest($2::bigint[], $3::bigint[]) \
         AS t(seq, item) ORDER BY t.seq",
    )
    .bind(mb)
    .bind(&seqs)
    .bind(&ids)
    .execute(&mut *c)
    .await?;
    Ok(refs)
}

/// Delivery-aware single insert: duplicate detection (mailbox row already locked) then insert.
async fn deliver_core(
    c: &mut PgConnection,
    mb: i64,
    msg: &NewMessage,
    delivery: Option<&Delivery>,
    knobs: Knobs,
) -> TxResult<DeliverOutcome> {
    if let Some(d) = delivery {
        // The mailbox row is locked: no concurrent delivery of this mailbox can
        // slip in between this lookup and the insert below. READ COMMITTED gives
        // this statement a fresh snapshot after the lock wait.
        let hit: Option<(i64, Vec<u8>)> = sqlx::query_as(&r(q::DELIVERY_LOOKUP))
            .bind(mb)
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
            let uid: i64 = sqlx::query_scalar(&r(q::MESSAGE_UID))
                .bind(mb)
                .bind(msg_id)
                .fetch_optional(&mut *c)
                .await?
                .ok_or_else(|| Error::storage("delivery refers to a missing message"))?;
            return Ok(DeliverOutcome::Duplicate(MessageRef {
                id: MessageId(msg_id),
                uid: to_uid(uid)?,
            }));
        }
    }
    let mut refs = insert_messages(c, mb, std::slice::from_ref(msg), delivery, knobs).await?;
    Ok(DeliverOutcome::Created(refs.remove(0)))
}

/// Fence and complete the job in the caller's transaction (see `JobQueue::complete_in`).
async fn complete_job(c: &mut PgConnection, f: JobFence) -> TxResult<()> {
    let n = sqlx::query(&r(q::COMPLETE_JOB))
        .bind(f.id.0)
        .bind(f.epoch)
        .execute(&mut *c)
        .await?
        .rows_affected();
    if n == 1 {
        Ok(())
    } else {
        Err(Error::Conflict("stale job epoch".into()).into())
    }
}

#[async_trait]
impl MailboxRepo for PgRepo {
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
        let id = tx_retry!(self, Some(mb), |c| {
                {
                    if let Some(p) = parent {
                        let rows: Vec<(i64, Option<i64>)> = sqlx::query_as(&r(q::ANCESTORS))
                            .bind(mb.0)
                            .bind(p.0)
                            .bind(MAX_FOLDER_DEPTH as i64)
                            .fetch_all(&mut *c)
                            .await?;
                        let depth = depth_verdict(&rows, None)?;
                        if depth + 1 > MAX_FOLDER_DEPTH {
                            return Err(Error::Invalid("folder tree too deep".into()).into());
                        }
                    }
                    let uv: i64 = sqlx::query_scalar(
                        "UPDATE mailboxes SET uid_validity = GREATEST(rs_now(), uid_validity + 1) \
                         WHERE id = $1 RETURNING uid_validity",
                    )
                    .bind(mb.0)
                    .fetch_one(&mut *c)
                    .await?;
                    if uv > i64::from(u32::MAX) {
                        return Err(Error::Quota("folder UIDVALIDITY space exhausted".into()).into());
                    }
                    let id: i64 = sqlx::query_scalar(&r(q::TAKE_FOLDER_ID))
                        .bind(mb.0)
                        .fetch_one(&mut *c)
                        .await?;
                    sqlx::query(&r(q::INSERT_FOLDER))
                        .bind(mb.0)
                        .bind(id)
                        .bind(parent.map(|p| p.0))
                        .bind(name)
                        .bind(uv)
                        .execute(&mut *c)
                        .await?;
                    let (kind, item_id) = ChangeItem::Folder(FolderId(id)).to_row();
                    let seq: i64 = sqlx::query_scalar(&r(q::TAKE_SEQ))
                        .bind(mb.0)
                        .fetch_one(&mut *c)
                        .await?;
                    sqlx::query(&r(q::INSERT_CHANGE))
                        .bind(mb.0)
                        .bind(seq)
                        .bind(kind)
                        .bind(item_id)
                        .bind(ChangeOp::Create.as_str())
                        .execute(&mut *c)
                        .await?;
                    Ok(id)
                }
        })?;
        Ok(FolderId(id))
    }

    async fn append(
        &self,
        mb: MailboxId,
        msg: &NewMessage,
        delivery: Option<&Delivery>,
        fence: Option<JobFence>,
    ) -> Result<DeliverOutcome> {
        let knobs = self.knobs;
        tx_retry!(self, Some(mb), |c| {
            {
                let outcome = deliver_core(c, mb.0, msg, delivery, knobs).await?;
                if let Some(f) = fence {
                    complete_job(c, f).await?;
                }
                Ok(outcome)
            }
        })
    }

    async fn append_many(&self, mb: MailboxId, msgs: &[NewMessage]) -> Result<Vec<MessageRef>> {
        let knobs = self.knobs;
        tx_retry!(self, Some(mb), |c| { insert_messages(c, mb.0, msgs, None, knobs).await })
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
