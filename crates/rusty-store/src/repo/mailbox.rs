//! `MailboxRepo`: the backend-neutral mailbox repository (PG4).
//!
//! # Boundary
//!
//! One method is ONE unit of work that owns its transaction: plain data in, plain data out, no
//! transaction handle crosses the trait. That is what lets the Postgres implementation re-run a
//! whole operation after `40P01` / `40001` (`tx_retry!`). The [`Mailbox`](crate::Mailbox) facade
//! keeps everything that is not a database concern: validation, size limits, MIME parsing, blob
//! IO (`blob.put` BEFORE the transaction, never repeated by a retry), paging limits and result
//! shaping. Both backends implement the same trait with their own SQL; statement text that is
//! identical up to the clock token lives in [`sql_text`].
//!
//! # Locking rule (enforced by tests)
//!
//! Every write method starts with the mailbox row lock (Postgres `SELECT .. FROM mailboxes WHERE
//! id = $1 FOR UPDATE`, SQLite `BEGIN IMMEDIATE` then the same row read). There is NO exemption:
//! every write method of this trait is mailbox scoped. `query_shape_pg.rs` records the statements
//! of each write method and asserts that the first data statement after `BEGIN` is the lock
//! (`set_config('lock_timeout', .., true)` is transaction-local configuration and precedes it).
//! After the mailbox row come the folder rows, then messages; `blobs` rows are touched only
//! through `lock_blobs` / `lock_existing_blobs` in ASCENDING hash order, in EVERY path that can
//! change a refcount: `append`, `append_many`, `deliver_many` (inserts), `delete_folder` and
//! `purge_deleted` (deletes). A move between folders does not change a refcount and a
//! soft-delete (`expunge`) neither.
//!
//! # Job fence inventory
//!
//! Operations a job step may perform atomically take `fence: Option<JobFence>`: the job is
//! fenced (epoch, `running`, lease unexpired by the DATABASE clock) and marked done in the same
//! transaction as the effect, also when the effect turns out to be a duplicate; a stale fence is
//! `Conflict` and rolls everything back. Today those are `append` (hence `deliver`) and
//! `deliver_many`; no other operation is used by a worker kind (checked against `rusty-store`'s
//! `JobQueue` call sites and the workspace: there is no worker crate yet). PG5 re-checks this
//! inventory when it converts the remaining `complete_in` call sites.
//!
//! # Post-commit hook point
//!
//! Every write returns [`Committed`]: the value plus the change-log `seq` of the LAST change the
//! transaction wrote (`None` when nothing was logged). A cache or notification hook (Valkey
//! plan) runs after the final commit, outside the retry loop, with that seq. No hook is
//! implemented here.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use rusty_core::{
    AccountId, BlobHash, DeliveryId, Error, Flags, FolderId, JobId, MailboxId, MessageId, Result,
    ThreadId,
};
use rusty_mime::Envelope;

use crate::mailbox::{
    BatchOutcome, ChangePage, CopyBatch, DeliverOutcome, MessageRef, MessageSummary, MoveBatch,
    MAX_FOLDER_DEPTH, MAX_THREAD_CANDIDATES,
};

// ---------------------------------------------------------------- neutral data

/// A message to store: metadata parsed by the facade, content already in the blob store.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub folder: FolderId,
    pub hash: BlobHash,
    pub size: i64,
    pub flags: Flags,
    pub env: Envelope,
    /// The client's internal date (IMAP APPEND); `None` = database clock. The facade bounds it with
    /// `MAX_SUPPLIED_TIME`. Honoured by `append` (a delivery's own `created_at` wins).
    pub internal_date: Option<i64>,
}

/// Idempotency key of a delivery. `created_at: None` = database clock; `Some(t)` is stored
/// verbatim as `deliveries.created_at` AND as the message's `internal_date` (test twins and, one
/// day, a client IMAP APPEND date; the facade bounds it with `MAX_SUPPLIED_TIME`).
#[derive(Debug, Clone, Copy)]
pub struct Delivery {
    pub id: DeliveryId,
    pub created_at: Option<i64>,
}

/// One item of `deliver_many`: the message and its idempotency key.
#[derive(Debug, Clone)]
pub struct DeliverEntry {
    pub msg: NewMessage,
    pub delivery_id: DeliveryId,
}

/// A claimed job to fence and complete in the SAME transaction as the repository effect (the
/// `JobQueue::complete_in` pattern without exposing a transaction handle).
#[derive(Debug, Clone, Copy)]
pub struct JobFence {
    pub id: JobId,
    pub epoch: i64,
}

/// The result of a write plus the last change-log seq it committed (post-commit hook point).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed<T> {
    pub value: T,
    /// `seq` of the last change this transaction logged; `None` if it logged nothing.
    pub committed_seq: Option<i64>,
}

impl<T> Committed<T> {
    pub(crate) fn new(value: T, committed_seq: Option<i64>) -> Self {
        Self {
            value,
            committed_seq,
        }
    }
}

/// A folder as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderInfo {
    pub id: FolderId,
    pub parent: Option<FolderId>,
    pub name: String,
    /// `inbox`, `sent`, `drafts`, `trash`, `spam`, `archive` or `custom`.
    pub role: String,
    /// IMAP UIDVALIDITY of the folder: never 0, and a folder id that is deleted and created again
    /// gets a different value.
    pub uid_validity: u32,
    /// The uid the next message of the folder receives (IMAP UIDNEXT).
    pub uid_next: u32,
    /// IMAP SUBSCRIBE state.
    pub subscribed: bool,
}

/// Result of `update_flags_many`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagUpdate {
    /// `(id, new flags)` in input order.
    pub updated: Vec<(MessageId, Flags)>,
    pub missing: Vec<MessageId>,
}

pub(crate) fn flag_update(applied: &[i64], now: &std::collections::HashMap<i64, i64>, missing: Vec<MessageId>) -> Result<FlagUpdate> {
    let mut updated = Vec::with_capacity(applied.len());
    for id in applied {
        let bits = now.get(id).copied().ok_or_else(|| Error::storage("missing flags row"))?;
        updated.push((MessageId(*id), crate::mailbox::to_flags(bits)?));
    }
    Ok(FlagUpdate { updated, missing })
}

/// Live message counts of one folder (IMAP STATUS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderCounts {
    pub messages: u32,
    /// Messages without the `\Seen` flag.
    pub unseen: u32,
}

/// One message with its location (soft-deleted messages included, see `deleted`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDetail {
    pub summary: MessageSummary,
    pub folder: FolderId,
    pub blob: BlobHash,
    pub deleted: bool,
}

/// Where the raw bytes of a message live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRef {
    pub blob: BlobHash,
    pub size: i64,
}

#[async_trait]
pub trait MailboxRepo: Send + Sync {
    // ---- lookups (no transaction)
    async fn mailbox_for_account(&self, account: AccountId) -> Result<MailboxId>;
    /// `NotFound` if the folder is not in this mailbox.
    async fn folder_exists(&self, mb: MailboxId, folder: FolderId) -> Result<()>;
    /// All folders in id order.
    async fn list_folders(&self, mb: MailboxId) -> Result<Vec<FolderInfo>>;
    /// Live and unseen message counts of one folder (`NotFound` if it is not in this mailbox).
    async fn folder_counts(&self, mb: MailboxId, folder: FolderId) -> Result<FolderCounts>;

    // ---- folders
    async fn create_folder(
        &self,
        mb: MailboxId,
        parent: Option<FolderId>,
        name: &str,
    ) -> Result<Committed<FolderId>>;
    /// Custom folders only (system folders are `Invalid`).
    async fn rename_folder(&self, mb: MailboxId, id: FolderId, name: &str)
        -> Result<Committed<()>>;
    /// Moving into itself or a descendant is `Conflict`; deeper than `MAX_FOLDER_DEPTH` is
    /// `Invalid`.
    async fn move_folder(
        &self,
        mb: MailboxId,
        id: FolderId,
        new_parent: Option<FolderId>,
    ) -> Result<Committed<()>>;
    /// Delete an empty custom folder; soft-deleted rows inside it are purged (their blobs are
    /// locked in ascending hash order first).
    async fn delete_folder(&self, mb: MailboxId, id: FolderId) -> Result<Committed<()>>;
    /// Set the IMAP subscription state (idempotent; `NotFound` for a folder of another mailbox).
    /// A folder update is logged in the change log.
    async fn set_subscribed(
        &self,
        mb: MailboxId,
        id: FolderId,
        subscribed: bool,
    ) -> Result<Committed<()>>;

    // ---- messages
    /// Store one message (plus a `deliveries` row if `delivery` is given), one transaction. With
    /// `fence` the job is fenced and marked done in the same transaction, also when the delivery
    /// turns out to be a duplicate; a stale fence rolls everything back (`Conflict`).
    async fn append(
        &self,
        mb: MailboxId,
        msg: &NewMessage,
        delivery: Option<&Delivery>,
        fence: Option<JobFence>,
    ) -> Result<Committed<DeliverOutcome>>;
    /// Store `msgs` (1..=MAX_BATCH) in input order in ONE transaction: ids, uids and change seqs
    /// are consecutive in input order.
    async fn append_many(
        &self,
        mb: MailboxId,
        msgs: &[NewMessage],
    ) -> Result<Committed<Vec<MessageRef>>>;
    /// Idempotent batch (`items` 1..=MAX_BATCH, all for the same folder). Outcomes equal a loop
    /// of `append(.., Some(delivery))`, including a delivery id repeated inside the batch.
    /// `at: Some(t)` is stored verbatim as `internal_date` / `created_at` of every created item.
    /// All or nothing: a delivery id reused with different content is `Conflict` for the batch.
    async fn deliver_many(
        &self,
        mb: MailboxId,
        items: &[DeliverEntry],
        at: Option<i64>,
        fence: Option<JobFence>,
    ) -> Result<Committed<Vec<DeliverOutcome>>>;
    async fn set_flags(&self, mb: MailboxId, id: MessageId, flags: Flags) -> Result<Committed<()>>;
    /// `ids`: 1..=MAX_BATCH, no duplicates (the facade dedups).
    /// Delta flag update (IMAP STORE +FLAGS / -FLAGS): `flags = (flags | add) & ~remove` on the LIVE
    /// value, atomically, so a stale snapshot never overwrites another session's change. Returns the
    /// new flags of each updated message in input order; ids that are not live messages are `missing`.
    /// `ids`: 1..=MAX_BATCH, no duplicates.
    async fn update_flags_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        add: Flags,
        remove: Flags,
    ) -> Result<Committed<FlagUpdate>>;
    /// Soft-delete the messages among `ids` whose live flags still contain Deleted (IMAP EXPUNGE must
    /// not destroy a message another session un-deleted); the others are `missing`.
    async fn expunge_deleted_many(&self, mb: MailboxId, ids: &[i64]) -> Result<Committed<BatchOutcome>>;
    async fn set_flags_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        flags: Flags,
    ) -> Result<Committed<BatchOutcome>>;
    /// `keyword` is already normalized (lowercase). A 33rd distinct keyword is `Quota`.
    async fn add_keyword(&self, mb: MailboxId, id: MessageId, keyword: &str)
        -> Result<Committed<()>>;
    async fn remove_keyword(
        &self,
        mb: MailboxId,
        id: MessageId,
        keyword: &str,
    ) -> Result<Committed<()>>;
    async fn move_message(
        &self,
        mb: MailboxId,
        id: MessageId,
        to: FolderId,
    ) -> Result<Committed<MessageRef>>;
    async fn move_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        to: FolderId,
    ) -> Result<Committed<MoveBatch>>;
    /// Copy live messages into `to`: each copy is a NEW message (fresh id, consecutive uids in
    /// input order, one `create` change each) that shares the blob and carries flags, keywords,
    /// internal date, envelope fields and thread of its source. `ids`: 1..=MAX_BATCH, no
    /// duplicates (the facade dedups). Ids that are not live messages of this mailbox are
    /// `missing`; an unknown destination is `NotFound`. Blob rows are locked in ascending hash
    /// order before the refcount trigger fires.
    async fn copy_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        to: FolderId,
    ) -> Result<Committed<CopyBatch>>;
    async fn expunge(&self, mb: MailboxId, id: MessageId) -> Result<Committed<()>>;
    /// `at: Some(t)` is stored verbatim as `deleted_at` (tests), `None` is the database clock.
    async fn expunge_many(
        &self,
        mb: MailboxId,
        ids: &[i64],
        at: Option<i64>,
    ) -> Result<Committed<BatchOutcome>>;
    /// `limit` must not be negative (`Invalid`). Hard-delete up to `limit` soft-deleted messages of THIS mailbox with `deleted_at <
    /// older_than` that have no `deliveries` row; returns how many. Blob refcounts follow; the
    /// blobs are locked in ascending hash order before the delete.
    async fn purge_deleted(
        &self,
        mb: MailboxId,
        older_than: i64,
        limit: i64,
    ) -> Result<Committed<u64>>;

    // ---- reads
    async fn get_message(&self, mb: MailboxId, id: MessageId) -> Result<MessageDetail>;
    /// Blob hash and size of a live message.
    async fn get_raw_ref(&self, mb: MailboxId, id: MessageId) -> Result<RawRef>;
    /// Keywords of a live message, sorted.
    async fn keywords(&self, mb: MailboxId, id: MessageId) -> Result<Vec<String>>;
    /// Keywords of the live messages among `ids` (1..=MAX_BATCH, distinct): `(message id, keyword)`
    /// rows ordered by message id then keyword; messages without keywords have no row.
    async fn keywords_many(&self, mb: MailboxId, ids: &[i64]) -> Result<Vec<(i64, String)>>;
    /// The change seq of the last change logged in this mailbox (`0` if none).
    async fn latest_change_seq(&self, mb: MailboxId) -> Result<i64>;
    async fn list_folder(
        &self,
        mb: MailboxId,
        folder: FolderId,
        after_uid: u32,
        limit: i64,
    ) -> Result<Vec<MessageSummary>>;
    async fn changes_since(&self, mb: MailboxId, seq: i64, limit: i64) -> Result<ChangePage>;
}

// ---------------------------------------------------------------- shared SQL

/// Statement text shared by the backends. Written once with numbered `$N` placeholders (valid
/// on Postgres natively and on SQLite through sqlx) and `{NOW}` for the database clock;
/// `render_*` produce the dialect. Only static text is ever rendered: caller data is always
/// bound.
pub mod sql_text {
    pub const FOLDER_EXISTS: &str = "SELECT id FROM folders WHERE mailbox_id = $1 AND id = $2";
    pub const MAILBOX_FOR_ACCOUNT: &str = "SELECT id FROM mailboxes WHERE account_id = $1";
    pub const LIST_FOLDERS: &str = "SELECT id, parent_id, name, role, uid_validity, next_uid, subscribed \
        FROM folders WHERE mailbox_id = $1 ORDER BY id";
    pub const FOLDER_COUNTS: &str = "SELECT COUNT(*), COUNT(CASE WHEN (flags & 1) = 0 THEN 1 END) \
        FROM messages WHERE mailbox_id = $1 AND folder_id = $2 AND deleted_at IS NULL";
    pub const SET_SUBSCRIBED: &str = "UPDATE folders SET subscribed = $3 WHERE mailbox_id = $1 AND id = $2";
    /// Copy one live message row to message id `$3`, folder `$4`, uid `$5`.
    pub const COPY_ROW: &str = "INSERT INTO messages (mailbox_id, id, folder_id, uid, blob_hash, size, \
        internal_date, subject, from_addr, to_addrs, message_id_hdr, thread_id, flags) \
        SELECT mailbox_id, CAST($3 AS BIGINT), CAST($4 AS BIGINT), CAST($5 AS BIGINT), blob_hash, size, \
        internal_date, subject, from_addr, to_addrs, message_id_hdr, thread_id, flags \
        FROM messages WHERE mailbox_id = $1 AND id = $2 AND deleted_at IS NULL";
    pub const COPY_KEYWORDS: &str = "INSERT INTO keywords (mailbox_id, message_id, keyword) \
        SELECT mailbox_id, CAST($3 AS BIGINT), keyword FROM keywords \
        WHERE mailbox_id = $1 AND message_id = $2";
    pub const FOLDER_ROW: &str = "SELECT parent_id, role FROM folders WHERE mailbox_id = $1 AND id = $2";
    pub const ANCESTORS: &str = "WITH RECURSIVE up(id, parent_id, depth) AS ( \
          SELECT id, parent_id, 1 FROM folders WHERE mailbox_id = $1 AND id = $2 \
          UNION ALL \
          SELECT f.id, f.parent_id, u.depth + 1 FROM folders f JOIN up u \
            ON f.mailbox_id = $1 AND f.id = u.parent_id \
          WHERE u.depth <= $3 \
        ) SELECT id, parent_id FROM up ORDER BY depth";
    pub const SUBTREE_HEIGHT: &str = "WITH RECURSIVE sub(id, depth) AS ( \
          SELECT CAST($1 AS BIGINT), CAST(1 AS BIGINT) \
          UNION ALL \
          SELECT f.id, s.depth + 1 FROM folders f JOIN sub s \
            ON f.mailbox_id = $2 AND f.parent_id = s.id \
          WHERE s.depth <= $3 \
        ) SELECT MAX(depth) FROM sub";
    pub const INSERT_FOLDER: &str = "INSERT INTO folders \
        (mailbox_id, id, parent_id, name, role, uid_validity, subscribed) \
        VALUES ($1, $2, $3, $4, 'custom', $5, 0)";
    pub const RENAME_FOLDER: &str = "UPDATE folders SET name = $3 WHERE mailbox_id = $1 AND id = $2";
    pub const REPARENT_FOLDER: &str =
        "UPDATE folders SET parent_id = $3 WHERE mailbox_id = $1 AND id = $2";
    pub const CHILD_COUNT: &str =
        "SELECT COUNT(*) FROM folders WHERE mailbox_id = $1 AND parent_id = $2";
    pub const LIVE_COUNT: &str = "SELECT COUNT(*) FROM messages \
        WHERE mailbox_id = $1 AND folder_id = $2 AND deleted_at IS NULL";
    pub const DELETE_FOLDER_MESSAGES: &str =
        "DELETE FROM messages WHERE mailbox_id = $1 AND folder_id = $2 AND deleted_at IS NOT NULL";
    pub const DELETE_FOLDER: &str = "DELETE FROM folders WHERE mailbox_id = $1 AND id = $2";
    pub const INSERT_CHANGE: &str =
        "INSERT INTO change_log (mailbox_id, seq, kind, item_id, op) VALUES ($1, $2, $3, $4, $5)";
    /// Allocate `$3` uids of a folder; returns the first.
    pub const TAKE_UIDS: &str = "UPDATE folders SET next_uid = next_uid + $3 \
        WHERE mailbox_id = $1 AND id = $2 RETURNING next_uid - $3";
    /// Allocate `$2` message ids AND `$2` change seqs (one per created message); returns both
    /// firsts. Message creation always consumes one of each, so one statement serves a batch.
    pub const TAKE_MSG_IDS_AND_SEQS: &str = "UPDATE mailboxes \
        SET next_msg_id = next_msg_id + $2, next_change_seq = next_change_seq + $2 \
        WHERE id = $1 RETURNING next_msg_id - $2, next_change_seq - $2";
    /// Allocate `$2` change seqs; returns the first.
    pub const TAKE_SEQS: &str = "UPDATE mailboxes SET next_change_seq = next_change_seq + $2 \
        WHERE id = $1 RETURNING next_change_seq - $2";
    pub const TAKE_FOLDER_ID: &str = "UPDATE mailboxes SET next_folder_id = next_folder_id + 1 \
        WHERE id = $1 RETURNING next_folder_id - 1";
    pub const TAKE_SEQ: &str = "UPDATE mailboxes SET next_change_seq = next_change_seq + 1 \
        WHERE id = $1 RETURNING next_change_seq - 1";
    pub const DELIVERY_LOOKUP: &str =
        "SELECT message_id, blob_hash FROM deliveries WHERE mailbox_id = $1 AND delivery_id = $2";
    pub const COMPLETE_JOB: &str = "UPDATE jobs SET state = 'done', lease_owner = NULL, \n        lease_expires_at = NULL, updated_at = {NOW} \n        WHERE id = $1 AND epoch = $2 AND state = 'running' AND lease_expires_at > {NOW} \
        AND (domain_id IS NULL OR domain_id = (SELECT a.domain_id FROM mailboxes m \
          JOIN accounts a ON a.id = m.account_id WHERE m.id = $3))";
    /// Run only after `COMPLETE_JOB` matched no row: does the fence still own its job (so the
    /// refusal was the domain binding and not a stale epoch)?
    pub const JOB_STILL_OWNED: &str = "SELECT CAST(1 AS BIGINT) FROM jobs \
        WHERE id = $1 AND epoch = $2 AND state = 'running' AND lease_expires_at > {NOW}";
    pub const MESSAGE_UID: &str = "SELECT uid FROM messages WHERE mailbox_id = $1 AND id = $2";
    pub const LIVE_MESSAGE: &str = "SELECT folder_id, uid FROM messages \
        WHERE mailbox_id = $1 AND id = $2 AND deleted_at IS NULL";
    pub const SET_FLAGS: &str = "UPDATE messages SET flags = $3 \
        WHERE mailbox_id = $1 AND id = $2 AND deleted_at IS NULL";
    pub const MOVE_MESSAGE: &str =
        "UPDATE messages SET folder_id = $3, uid = $4 WHERE mailbox_id = $1 AND id = $2";
    pub const EXPUNGE: &str = "UPDATE messages SET deleted_at = {NOW} \
        WHERE mailbox_id = $1 AND id = $2 AND deleted_at IS NULL";
    pub const KEYWORD_PRESENT: &str =
        "SELECT CAST(1 AS BIGINT) FROM keywords WHERE mailbox_id = $1 AND message_id = $2 AND keyword = $3";
    pub const KEYWORD_COUNT: &str =
        "SELECT COUNT(*) FROM keywords WHERE mailbox_id = $1 AND message_id = $2";
    pub const KEYWORD_INSERT: &str =
        "INSERT INTO keywords (mailbox_id, message_id, keyword) VALUES ($1, $2, $3)";
    pub const KEYWORD_DELETE: &str =
        "DELETE FROM keywords WHERE mailbox_id = $1 AND message_id = $2 AND keyword = $3";
    pub const LATEST_SEQ: &str = "SELECT next_change_seq - 1 FROM mailboxes WHERE id = $1";
    pub const KEYWORDS_OF: &str = "SELECT keyword FROM keywords \
        WHERE mailbox_id = $1 AND message_id = $2 ORDER BY keyword";
    pub const LIST_FOLDER: &str = "SELECT id, uid, size, internal_date, subject, from_addr, \
        flags, thread_id FROM messages WHERE mailbox_id = $1 AND folder_id = $2 AND uid > $3 \
        AND deleted_at IS NULL ORDER BY uid LIMIT $4";
    pub const CHANGES: &str = "SELECT seq, kind, item_id, op FROM change_log \
        WHERE mailbox_id = $1 AND seq > $2 ORDER BY seq LIMIT $3";
}

// ---------------------------------------------------------------- pure policy

/// Verdict of the ancestor walk (rows from `sql_text::ANCESTORS` in depth order). Identical on
/// every backend: the database only returns rows, the policy is Rust.
pub(crate) fn depth_verdict(rows: &[(i64, Option<i64>)], forbidden: Option<i64>) -> Result<usize> {
    let mut depth = 0usize;
    let mut parent = None;
    for (id, p) in rows {
        if forbidden == Some(*id) {
            return Err(Error::Conflict(
                "cannot move a folder into itself or a descendant".into(),
            ));
        }
        depth += 1;
        if depth > MAX_FOLDER_DEPTH {
            return Err(Error::Invalid("folder tree too deep".into()));
        }
        parent = *p;
    }
    // No rows: `start` is not in this mailbox. A parent that is not there either (corrupt
    // data) ends the walk early with the same answer.
    if depth == 0 || parent.is_some() {
        return Err(Error::NotFound("folder".into()));
    }
    Ok(depth)
}

/// Message-ids to look up for the thread of `env`, nearest ancestor first.
pub(crate) fn thread_candidates(env: &Envelope) -> Vec<&str> {
    env.references
        .iter()
        .rev()
        .chain(env.in_reply_to.iter())
        .take(MAX_THREAD_CANDIDATES)
        .map(String::as_str)
        .collect()
}

pub(crate) fn not_found_folder() -> Error {
    Error::NotFound("folder".into())
}

pub(crate) fn not_found_mailbox() -> Error {
    Error::NotFound("mailbox".into())
}

pub(crate) fn not_found_message() -> Error {
    Error::NotFound("message".into())
}

/// `Invalid` unless `1..=MAX_BATCH` items (the facade checks first; a repository is a second
/// line of defence for direct callers).
pub(crate) fn check_batch_len(n: usize) -> Result<()> {
    if n == 0 || n > crate::mailbox::MAX_BATCH {
        return Err(Error::Invalid("a batch holds between 1 and 1000 items".into()));
    }
    Ok(())
}

/// `Invalid` when `ids` holds a duplicate: a batch move would allocate and log twice for one row.
/// The facade dedups; this is the repository's second line of defence, identical on both backends.
pub(crate) fn check_distinct_ids(ids: &[i64]) -> Result<()> {
    let mut seen = HashSet::with_capacity(ids.len());
    if ids.iter().all(|id| seen.insert(*id)) {
        Ok(())
    } else {
        Err(Error::Invalid("duplicate ids in a batch".into()))
    }
}

/// `Invalid` for a negative purge limit (Postgres rejects it, SQLite used to read it as unlimited).
pub(crate) fn check_purge_limit(limit: i64) -> Result<()> {
    if limit < 0 {
        return Err(Error::Invalid("purge limit must not be negative".into()));
    }
    Ok(())
}

/// Split `ids` (input order) into those in `hit` and the rest.
pub(crate) fn split_hits(
    ids: &[i64],
    hit: &std::collections::HashSet<i64>,
) -> (Vec<i64>, Vec<MessageId>) {
    let (mut applied, mut missing) = (Vec::new(), Vec::new());
    for id in ids {
        if hit.contains(id) {
            applied.push(*id);
        } else {
            missing.push(MessageId(*id));
        }
    }
    missing.truncate(crate::mailbox::MAX_BATCH);
    (applied, missing)
}

/// How one input item of `deliver_many` resolves.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Slot {
    /// The delivery id exists already: the message with this id.
    Existing(i64),
    /// The n-th message created by this batch.
    Created(usize),
    /// A repeat inside the batch of the n-th created message.
    DuplicateOfNew(usize),
}

pub(crate) struct DeliveryPlan {
    pub slots: Vec<Slot>,
    /// Item index of each message to create, in input order.
    pub creates: Vec<usize>,
}

/// Plan `deliver_many` in input order, as a loop of `append(.., Some(delivery))` would see it.
/// `existing` maps a delivery id to `(message id, blob hash hex)`. A delivery id used with
/// different content (against a stored delivery or an earlier item of the batch) is `Conflict`.
pub(crate) fn plan_deliveries(
    items: &[DeliverEntry],
    existing: &HashMap<Vec<u8>, (i64, String)>,
) -> Result<DeliveryPlan> {
    let reused = || Error::Conflict("delivery id reused with different content".into());
    let mut slots = Vec::with_capacity(items.len());
    let mut creates: Vec<usize> = Vec::new();
    let mut new_by_delivery: HashMap<[u8; 32], usize> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        if let Some((msg, hash)) = existing.get(&it.delivery_id.0[..]) {
            if hash != it.msg.hash.as_str() {
                return Err(reused());
            }
            slots.push(Slot::Existing(*msg));
        } else if let Some(&c) = new_by_delivery.get(&it.delivery_id.0) {
            if items[creates[c]].msg.hash != it.msg.hash {
                return Err(reused());
            }
            slots.push(Slot::DuplicateOfNew(c));
        } else {
            new_by_delivery.insert(it.delivery_id.0, creates.len());
            slots.push(Slot::Created(creates.len()));
            creates.push(i);
        }
    }
    Ok(DeliveryPlan { slots, creates })
}

/// Plan of `move_many`: `Some(uid)` = already in the destination, `None` = gets a new uid.
pub(crate) struct MovePlan {
    pub plan: Vec<(i64, Option<i64>)>,
    pub missing: Vec<MessageId>,
    pub to_move: Vec<i64>,
}

/// `live`: id -> (folder, uid) of the live messages among `ids`.
pub(crate) fn plan_move(ids: &[i64], live: &HashMap<i64, (i64, i64)>, to: FolderId) -> MovePlan {
    let mut plan: Vec<(i64, Option<i64>)> = Vec::new();
    let mut missing = Vec::new();
    for id in ids {
        match live.get(id) {
            None => missing.push(MessageId(*id)),
            Some((folder, uid)) if *folder == to.0 => plan.push((*id, Some(*uid))),
            Some(_) => plan.push((*id, None)),
        }
    }
    missing.truncate(crate::mailbox::MAX_BATCH);
    let to_move: Vec<i64> = plan.iter().filter(|p| p.1.is_none()).map(|p| p.0).collect();
    MovePlan {
        plan,
        missing,
        to_move,
    }
}

/// Final `(id, ref)` list of a move: moved messages get consecutive uids from `first_uid`.
pub(crate) fn moved_refs(
    plan: Vec<(i64, Option<i64>)>,
    first_uid: i64,
) -> Result<Vec<(MessageId, MessageRef)>> {
    use crate::mailbox::to_uid;
    let mut next = first_uid;
    let mut moved = Vec::with_capacity(plan.len());
    for (id, existing) in plan {
        let uid = match existing {
            Some(u) => to_uid(u)?,
            None => {
                next += 1;
                to_uid(next - 1)?
            }
        };
        moved.push((
            MessageId(id),
            MessageRef {
                id: MessageId(id),
                uid,
            },
        ));
    }
    Ok(moved)
}

/// Row of `list_folders` (`sql_text::LIST_FOLDERS`) to the DTO.
pub(crate) type FolderRow = (i64, Option<i64>, String, String, i64, i64, i64);

pub(crate) fn folder_from_row(row: FolderRow) -> Result<FolderInfo> {
    let (id, parent, name, role, uid_validity, next_uid, subscribed) = row;
    Ok(FolderInfo {
        id: FolderId(id),
        parent: parent.map(FolderId),
        name,
        role,
        uid_validity: u32::try_from(uid_validity).map_err(|_| Error::storage("corrupt uid_validity"))?,
        // next_uid may sit one past u32::MAX once the uid space is exhausted; clamp for display.
        uid_next: u32::try_from(next_uid).unwrap_or(u32::MAX),
        subscribed: subscribed != 0,
    })
}

pub(crate) fn counts_from_row(row: (i64, i64)) -> Result<FolderCounts> {
    let conv = |v| u32::try_from(v).map_err(|_| Error::storage("corrupt message count"));
    Ok(FolderCounts { messages: conv(row.0)?, unseen: conv(row.1)? })
}

/// Row of `list_folder` (`sql_text::LIST_FOLDER`) to the DTO.
pub(crate) type SummaryRow = (i64, i64, i64, i64, String, String, i64, i64);

pub(crate) fn summary_from_row(row: SummaryRow) -> Result<MessageSummary> {
    use crate::mailbox::{to_flags, to_uid};
    let (id, uid, size, internal_date, subject, from_addr, flags, thread) = row;
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
}

/// `changes_since` written ONCE over `sqlx::Database` instead of once per backend (plain read
/// of primitive columns; see the PG2 report for why writes are not generalised).
pub(crate) async fn changes_since_generic<'e, DB, E>(
    ex: E,
    sql: &str,
    mb: MailboxId,
    seq: i64,
    limit: i64,
) -> Result<ChangePage>
where
    DB: sqlx::Database,
    E: sqlx::Executor<'e, Database = DB>,
    for<'q> <DB as sqlx::Database>::Arguments<'q>: sqlx::IntoArguments<'q, DB>,
    i64: sqlx::Type<DB> + for<'q> sqlx::Encode<'q, DB>,
    (i64, String, i64, String): for<'r> sqlx::FromRow<'r, DB::Row>,
{
    use crate::mailbox::{Change, ChangeItem, ChangeOp};
    let rows: Vec<(i64, String, i64, String)> = sqlx::query_as(sql)
        .bind(mb.0)
        .bind(seq)
        .bind(limit + 1)
        .fetch_all(ex)
        .await
        .map_err(crate::map_db_error)?;
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
