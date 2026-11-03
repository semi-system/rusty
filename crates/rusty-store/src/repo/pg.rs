//! Postgres implementation of the spike repositories.
//!
//! Transaction discipline (READ COMMITTED):
//! 1. `PgRepo::run` owns BEGIN / COMMIT and the retry loop. A unit of work is a closure that is
//!    re-run from scratch when Postgres reports a deadlock (40P01) or serialization failure
//!    (40001); after `MAX_ATTEMPTS` the error becomes a fixed `Storage` error.
//! 2. For mailbox writes the first statement is `SELECT .. FROM mailboxes WHERE id = $1 FOR
//!    UPDATE` (`run(Some(mb), ..)`), so writers of one mailbox are serialized and writers of
//!    different mailboxes run in parallel.
//! 3. `blobs` rows are locked in ASCENDING hash order (`lock_blobs`) BEFORE any statement that
//!    fires the refcount trigger, so two mailboxes storing overlapping blob sets cannot
//!    deadlock on `blobs`. The retry loop is the safety net, not the mechanism.
//! 4. Errors: statements return `sqlx::Error` through `TxError::Db` so the loop can look at the
//!    SQLSTATE; everything else is mapped by `map_db_error` (SQLSTATE only, never message text).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rand_core::{OsRng, RngCore};
use rusty_core::{
    AccountId, DomainId, Error, FolderId, MailboxId, MessageId, Result, ThreadId,
};
use sqlx::PgConnection;

use super::{
    depth_verdict, hash_bytes, not_found_folder, not_found_mailbox, render_pg as r,
    sql_text as q, thread_candidates, Delivery, DirectoryRepo, ListMember, MailboxRepo,
    NewAccount, NewMessage, Resolution, SYSTEM_FOLDERS, SYSTEM_FOLDER_TABLE,
};
use crate::mailbox::{
    to_flags, to_uid, Change, ChangeItem, ChangeOp, ChangePage, DeliverOutcome, MessageRef,
    MessageSummary, MAX_FOLDER_DEPTH,
};
use crate::{map_db_error, PgStore};

/// Attempts of one unit of work (first try included).
pub const MAX_ATTEMPTS: u32 = 5;

/// Error inside a unit of work: a database error (kept raw so its SQLSTATE can be inspected by
/// the retry loop) or an already-final domain error.
#[derive(Debug)]
pub enum TxError {
    Db(sqlx::Error),
    Domain(Error),
}

impl From<sqlx::Error> for TxError {
    fn from(e: sqlx::Error) -> Self {
        TxError::Db(e)
    }
}

impl From<Error> for TxError {
    fn from(e: Error) -> Self {
        TxError::Domain(e)
    }
}

type TxResult<T> = std::result::Result<T, TxError>;

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error().and_then(|d| d.code().map(|c| c.into_owned()))
}

/// 40P01 deadlock_detected, 40001 serialization_failure: the whole transaction may be re-run.
pub fn is_retryable(e: &sqlx::Error) -> bool {
    matches!(sqlstate(e).as_deref(), Some("40P01") | Some("40001"))
}

fn is_deadlock(e: &sqlx::Error) -> bool {
    sqlstate(e).as_deref() == Some("40P01")
}

/// Counters of the retry loop (process-lifetime, per `PgRepo`).
#[derive(Debug, Default)]
pub struct RetryStats {
    pub retries: AtomicU64,
    pub deadlocks: AtomicU64,
    pub exhausted: AtomicU64,
}

/// Test-only fault injection: lets a test force the interleaving that the ordering rule
/// prevents. Never set in production code paths.
#[derive(Debug, Clone, Copy, Default)]
pub struct Knobs {
    /// Lock blobs one by one in the order the messages are given instead of ascending.
    pub unordered_blob_locks: bool,
    /// `pg_sleep` this long after the first blob lock (only with `unordered_blob_locks`).
    pub hold_after_first_blob_ms: u32,
}

#[derive(Clone)]
pub struct PgRepo {
    store: PgStore,
    stats: Arc<RetryStats>,
    knobs: Knobs,
}

impl PgRepo {
    pub fn new(store: PgStore) -> Self {
        Self {
            store,
            stats: Arc::default(),
            knobs: Knobs::default(),
        }
    }

    #[doc(hidden)]
    pub fn with_knobs_for_test(mut self, knobs: Knobs) -> Self {
        self.knobs = knobs;
        self
    }

    pub fn stats(&self) -> &RetryStats {
        &self.stats
    }

    /// Begin a READ COMMITTED transaction and, if asked, lock the mailbox row FIRST
    /// (`SELECT .. FOR UPDATE`). `NotFound` if the mailbox does not exist.
    async fn begin_locked(
        &self,
        lock_mailbox: Option<MailboxId>,
    ) -> TxResult<sqlx::Transaction<'static, sqlx::Postgres>> {
        let mut tx = self.store.begin_write().await?;
        if let Some(mb) = lock_mailbox {
            sqlx::query_scalar::<_, i64>("SELECT id FROM mailboxes WHERE id = $1 FOR UPDATE")
                .bind(mb.0)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(not_found_mailbox)?;
        }
        Ok(tx)
    }

    /// Finish one attempt: commit on success; roll back and classify on failure.
    async fn finish<T>(
        &self,
        tx: sqlx::Transaction<'static, sqlx::Postgres>,
        res: TxResult<T>,
        attempt: u32,
    ) -> Step<T> {
        let failure = match res {
            Ok(v) => match tx.commit().await {
                Ok(()) => return Step::Done(v),
                Err(e) => TxError::Db(e),
            },
            Err(e) => {
                // The transaction may already be aborted; the rollback result is moot.
                let _ = tx.rollback().await;
                e
            }
        };
        match failure {
            TxError::Db(e) if is_retryable(&e) => {
                self.stats.retries.fetch_add(1, Ordering::Relaxed);
                if is_deadlock(&e) {
                    self.stats.deadlocks.fetch_add(1, Ordering::Relaxed);
                }
                if attempt + 1 >= MAX_ATTEMPTS {
                    self.stats.exhausted.fetch_add(1, Ordering::Relaxed);
                    return Step::Fail(Error::storage("transaction retry limit exceeded"));
                }
                backoff(attempt).await;
                Step::Retry
            }
            TxError::Db(e) => Step::Fail(map_db_error(e)),
            TxError::Domain(e) => Step::Fail(e),
        }
    }
}

enum Step<T> {
    Done(T),
    Retry,
    Fail(Error),
}

/// Run `$body` (a block evaluating to `TxResult<T>`, with `$c: &mut PgConnection` in scope) in
/// a READ COMMITTED transaction, optionally locking the mailbox row first, retrying the WHOLE
/// block on 40P01/40001 up to `MAX_ATTEMPTS` times. The body may therefore run several times:
/// it must have no side effects outside the transaction (blob IO happens before the
/// repository call).
///
/// A macro instead of a closure-taking method: a closure `for<'c> FnMut(&'c mut PgConnection)
/// -> Pin<Box<dyn Future + 'c>>` cannot capture borrowed arguments of the calling
/// `async_trait` method (rustc: "returning this value requires that `'life1` must outlive
/// `'static`"). Inlining the loop keeps every borrow in one future.
macro_rules! tx_retry {
    ($self:expr, $lock:expr, |$c:ident| $body:block) => {{
        let mut attempt = 0u32;
        loop {
            let mut tx = $self.begin_locked($lock).await.map_err(|e| match e {
                TxError::Db(e) => map_db_error(e),
                TxError::Domain(e) => e,
            })?;
            let res: TxResult<_> = async {
                let $c: &mut PgConnection = &mut *tx;
                $body
            }
            .await;
            match $self.finish(tx, res, attempt).await {
                Step::Done(v) => break Ok::<_, Error>(v),
                Step::Retry => attempt += 1,
                Step::Fail(e) => break Err(e),
            }
        }
    }};
}

/// 5 ms * 2^attempt plus up to the same again of jitter.
async fn backoff(attempt: u32) {
    let base = 5u64 << attempt.min(6);
    let jitter = u64::from(OsRng.next_u32()) % (base + 1);
    tokio::time::sleep(Duration::from_millis(base + jitter)).await;
}

const UV_MAX_BASE: i64 = u32::MAX as i64 - SYSTEM_FOLDERS;

#[async_trait]
impl DirectoryRepo for PgRepo {
    async fn create_domain(&self, name: &str) -> Result<DomainId> {
        let id: i64 = sqlx::query_scalar(&r(q::INSERT_DOMAIN))
            .bind(name)
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(DomainId(id))
    }

    async fn create_account(&self, new: &NewAccount) -> Result<AccountId> {
        let ids: Vec<i64> = SYSTEM_FOLDER_TABLE.iter().map(|f| f.0).collect();
        let names: Vec<&str> = SYSTEM_FOLDER_TABLE.iter().map(|f| f.1).collect();
        let roles: Vec<&str> = SYSTEM_FOLDER_TABLE.iter().map(|f| f.2).collect();
        let acc = tx_retry!(self, None, |c| {
                {
                    let acc: i64 = sqlx::query_scalar(&r(q::INSERT_ACCOUNT))
                        .bind(&new.local)
                        .bind(new.domain.0)
                        .bind(new.plan.map(|p| p.0))
                        .bind(new.password_hash.as_deref())
                        .fetch_one(&mut *c)
                        .await?;
                    let (mb, uv_base): (i64, i64) = sqlx::query_as(
                        "INSERT INTO mailboxes (account_id, uid_validity, next_folder_id) \
                         VALUES ($1, LEAST(GREATEST(rs_now(), 1), $2) + $3, $4) \
                         RETURNING id, uid_validity - $3",
                    )
                    .bind(acc)
                    .bind(UV_MAX_BASE)
                    .bind(SYSTEM_FOLDERS)
                    .bind(SYSTEM_FOLDERS + 1)
                    .fetch_one(&mut *c)
                    .await?;
                    sqlx::query(
                        "INSERT INTO folders (mailbox_id, id, parent_id, name, role, uid_validity) \
                         SELECT $1, t.id, NULL, t.name, t.role, $2 + t.id \
                         FROM unnest($3::bigint[], $4::text[], $5::text[]) AS t(id, name, role)",
                    )
                    .bind(mb)
                    .bind(uv_base)
                    .bind(&ids)
                    .bind(&names)
                    .bind(&roles)
                    .execute(&mut *c)
                    .await?;
                    Ok(acc)
                }
        })?;
        Ok(AccountId(acc))
    }

    async fn resolve(
        &self,
        local: &str,
        domain: &str,
        max_members: usize,
    ) -> Result<Option<Resolution>> {
        let pool = self.store.pool();
        let Some((kind, id)): Option<(i64, i64)> = sqlx::query_as(&r(q::RESOLVE_KIND))
            .bind(domain)
            .bind(local)
            .fetch_optional(pool)
            .await
            .map_err(map_db_error)?
        else {
            return Ok(None);
        };
        if kind != 3 {
            return Ok(Some(Resolution::Account(AccountId(id))));
        }
        let rows: Vec<(Option<i64>, Option<String>)> = sqlx::query_as(&r(q::LIST_MEMBERS))
            .bind(id)
            .bind(max_members as i64 + 1)
            .fetch_all(pool)
            .await
            .map_err(map_db_error)?;
        if rows.len() > max_members {
            return Err(Error::Quota("list has too many members".into()));
        }
        Ok(Some(Resolution::List(
            rows.into_iter()
                .filter_map(|(a, e)| match (a, e) {
                    (Some(a), _) => Some(ListMember::Account(AccountId(a))),
                    (None, Some(e)) => Some(ListMember::External(e)),
                    _ => None,
                })
                .collect(),
        )))
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
    ) -> Result<DeliverOutcome> {
        let knobs = self.knobs;
        let one = std::slice::from_ref(msg);
        tx_retry!(self, Some(mb), |c| {
            {
                if let Some(d) = delivery {
                    // The mailbox row is locked: no concurrent delivery of this mailbox can
                    // slip in between this lookup and the insert below. READ COMMITTED gives
                    // this statement a fresh snapshot after the lock wait.
                    let hit: Option<(i64, Vec<u8>)> = sqlx::query_as(&r(q::DELIVERY_LOOKUP))
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
                        let uid: i64 = sqlx::query_scalar(&r(q::MESSAGE_UID))
                            .bind(mb.0)
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
                let mut refs = insert_messages(c, mb.0, one, delivery, knobs).await?;
                Ok(DeliverOutcome::Created(refs.remove(0)))
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
        let rows: Vec<(i64, String, i64, String)> = sqlx::query_as(&r(q::CHANGES))
            .bind(mb.0)
            .bind(seq)
            .bind(limit + 1)
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)?;
        let more = rows.len() as i64 > limit;
        let changes = rows
            .into_iter()
            .take(limit as usize)
            .map(|(seq, kind, item_id, op)| {
                Ok(Change {
                    seq,
                    item: ChangeItem::from_row(&kind, item_id)?,
                    op: ChangeOp::parse(&op)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ChangePage { changes, more })
    }
}
