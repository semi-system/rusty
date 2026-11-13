//! `Mailbox`: the high-level facade over one account's folders, messages and change log.
//!
//! The facade is backend-free: it holds an `Arc<dyn MailboxRepo>` and keeps validation, size
//! limits, MIME parsing, blob IO (`blob.put` BEFORE the metadata transaction, never repeated
//! by a retry), paging limits and result shaping. Every method is one repository call, i.e.
//! one transaction. See `repo::mailbox` for the repository contract.
//!
//! Invariants (enforced by the repositories, tested on both backends):
//! - Locking rule: every write transaction takes the mailbox row lock FIRST (SQLite `BEGIN
//!   IMMEDIATE` plus reading the row, Postgres `SELECT .. FOR UPDATE`; a missing row is
//!   `NotFound`), then folders, then messages; `blobs` rows are locked in ascending hash order
//!   in every path that changes a refcount.
//! - The counters `mailboxes.next_msg_id`, `mailboxes.next_change_seq`,
//!   `mailboxes.next_folder_id` and `folders.next_uid` are incremented inside that transaction,
//!   so message ids, uids and change seqs are gapless and monotonic, and folder ids are never
//!   reused (a deleted folder's id is not handed out again).
//! - Reads are paginated: `list_folder` and `changes_since` return at most [`MAX_PAGE`]
//!   entries per call (a larger `limit` is clamped, `0` is `Invalid`).
//! - Every lookup is scoped by `mailbox_id`: ids of another mailbox are `NotFound`.
//! - `deliver` is `append` plus a `deliveries` row in the same transaction; the duplicate
//!   lookup runs under the mailbox lock, so it is race-free.
//! - `append` writes the blob before the metadata transaction; if the transaction fails the
//!   blob is an orphan for the cleanup job (spec §5).
//! - Batch calls (`set_flags_many`, `move_many`, `expunge_many`, `deliver_many`) are one
//!   transaction with set-based statements and one `UPDATE ... RETURNING` per counter.
//! - Clock policy D2: correctness timestamps come from the database clock; the `_at` twins
//!   (`#[doc(hidden)]`) store a caller value verbatim, bounded by [`MAX_SUPPLIED_TIME`].
//! - Nothing here logs message content; error texts never echo caller input.
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use rusty_core::{
    AccountId, DeliveryId, Error, Flags, FolderId, MailboxId, MessageId, Result, ThreadId,
};
use rusty_mime::Envelope;

use crate::gc::PURGE_BATCH;
use crate::repo::{
    DeliverEntry, Delivery, FlagUpdate, FolderCounts, FolderInfo, JobFence, MailboxRepo, MessageDetail, NewMessage, RawRef,
};
use crate::BlobStore;

pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_KEYWORDS_PER_MESSAGE: usize = 32;
pub const MAX_KEYWORD_BYTES: usize = 255;
pub const MAX_FOLDER_NAME_BYTES: usize = 255;
/// Upper bound (unix seconds, 2100-01-01T00:00:00Z) for a caller-supplied timestamp: the `_at`
/// twins and, later, a client IMAP APPEND date. Stored verbatim (clock policy D2), so absurd
/// values are rejected with `Error::Invalid` rather than written. The lower bound is 0.
pub const MAX_SUPPLIED_TIME: i64 = 4_102_444_800;

pub(crate) fn check_supplied_time(t: i64) -> Result<()> {
    if (0..=MAX_SUPPLIED_TIME).contains(&t) {
        Ok(())
    } else {
        Err(Error::Invalid("timestamp out of range".into()))
    }
}
/// Maximum number of levels in the folder tree (a root folder is level 1).
pub const MAX_FOLDER_DEPTH: usize = 64;
/// Bound on parent lookups per appended message (hostile `References` headers).
pub(crate) const MAX_THREAD_CANDIDATES: usize = 32;
/// Most entries returned by one `list_folder` / `changes_since` call.
pub const MAX_PAGE: u32 = 1000;

/// Resource limits enforced by a `Mailbox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxLimits {
    /// Largest accepted raw message; larger appends fail with `Error::Quota`.
    pub max_message_bytes: usize,
}

impl Default for MailboxLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageRef {
    pub id: MessageId,
    pub uid: u32,
}

/// Result of [`Mailbox::deliver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeliverOutcome {
    /// First delivery of this id: the message was created.
    Created(MessageRef),
    /// A retry: the ORIGINAL message; nothing was written.
    Duplicate(MessageRef),
}

/// Most ids (or items) one batch call accepts; callers chunk larger work.
pub const MAX_BATCH: usize = 1000;
/// Most raw bytes one [`Mailbox::deliver_many`] call accepts in total (per-message limits
/// still apply).
pub const MAX_DELIVER_BATCH_BYTES: usize = 64 * 1024 * 1024;

/// Result of [`Mailbox::set_flags_many`] and [`Mailbox::expunge_many`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchOutcome {
    /// Messages changed (duplicates in the input count once).
    pub applied: u32,
    /// Ids that were not touched because they do not exist in this mailbox or are
    /// soft-deleted, in input order (at most [`MAX_BATCH`]).
    pub missing: Vec<MessageId>,
}

/// Result of [`Mailbox::move_many`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveBatch {
    /// `(id, new ref)` in input order. A message already in the destination is listed with
    /// its current ref and is not changed.
    pub moved: Vec<(MessageId, MessageRef)>,
    pub missing: Vec<MessageId>,
}

/// Result of [`Mailbox::copy_many`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyBatch {
    /// `(source id, ref of the new copy)` in input order.
    pub copied: Vec<(MessageId, MessageRef)>,
    /// Ids that are not live messages of this mailbox, in input order (at most [`MAX_BATCH`]).
    pub missing: Vec<MessageId>,
}

/// One message of [`Mailbox::deliver_many`].
#[derive(Debug, Clone)]
pub struct DeliverItem {
    pub raw: Bytes,
    pub flags: Flags,
    pub delivery_id: DeliveryId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSummary {
    pub id: MessageId,
    pub uid: u32,
    pub size: i64,
    pub internal_date: i64,
    pub subject: String,
    pub from_addr: String,
    pub flags: Flags,
    pub thread_id: ThreadId,
}

/// The item a change-log entry is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeItem {
    Message(MessageId),
    Folder(FolderId),
}

impl ChangeItem {
    /// `(change_log.kind, change_log.item_id)`.
    pub(crate) fn to_row(self) -> (&'static str, i64) {
        match self {
            ChangeItem::Message(id) => ("message", id.0),
            ChangeItem::Folder(id) => ("folder", id.0),
        }
    }

    pub(crate) fn from_row(kind: &str, item_id: i64) -> Result<ChangeItem> {
        match kind {
            "message" => Ok(ChangeItem::Message(MessageId(item_id))),
            "folder" => Ok(ChangeItem::Folder(FolderId(item_id))),
            _ => Err(Error::storage("corrupt change_log kind")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeOp {
    Create,
    Update,
    Destroy,
}

impl ChangeOp {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ChangeOp::Create => "create",
            ChangeOp::Update => "update",
            ChangeOp::Destroy => "destroy",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<ChangeOp> {
        match s {
            "create" => Ok(ChangeOp::Create),
            "update" => Ok(ChangeOp::Update),
            "destroy" => Ok(ChangeOp::Destroy),
            _ => Err(Error::storage("corrupt change_log op")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Change {
    pub seq: i64,
    pub item: ChangeItem,
    pub op: ChangeOp,
}

/// One page of [`Mailbox::changes_since`]: entries in seq order, and whether more entries
/// follow the last one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangePage {
    pub changes: Vec<Change>,
    pub more: bool,
}

/// Effective page size: `Invalid` for 0, at most [`MAX_PAGE`].
pub(crate) fn page_limit(limit: u32) -> Result<i64> {
    if limit == 0 {
        return Err(Error::Invalid("page limit must be at least 1".into()));
    }
    Ok(i64::from(limit.min(MAX_PAGE)))
}

/// Told after every committed write of a mailbox (the post-commit hook point of `MailboxRepo`):
/// `seq` is the change-log seq of the last change the write logged. The one place where "this
/// mailbox changed" is announced is this facade, so every writer (SMTP delivery, IMAP, a later JMAP
/// service) notifies without a call site of its own. Implementations must never block.
pub trait ChangeNotifier: Send + Sync {
    fn changed(&self, mailbox: MailboxId, seq: i64);
}

/// One account's mailbox. Cheap to clone; clones share the pool and blob store.
///
/// Invariant: every code path that inserts a row into `mailboxes` must also seed
/// `next_folder_id` (migration 0003 column; `take_folder_id` hands out folder ids from it),
/// or `create_folder` would produce colliding or missing ids.
#[derive(Clone)]
pub struct Mailbox {
    repo: Arc<dyn MailboxRepo>,
    blobs: Arc<dyn BlobStore>,
    id: MailboxId,
    limits: MailboxLimits,
    notifier: Option<Arc<dyn ChangeNotifier>>,
}

impl fmt::Debug for Mailbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mailbox")
            .field("id", &self.id)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

// ---------- validation ----------

/// Folder names: 1..=255 bytes, no '/', no control characters, not "." or "..".
pub fn validate_folder_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_FOLDER_NAME_BYTES {
        return Err(Error::Invalid("folder name must be 1..=255 bytes".into()));
    }
    if name.contains('/') || name.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "folder name contains a forbidden character".into(),
        ));
    }
    if name == "." || name == ".." {
        return Err(Error::Invalid("folder name must not be . or ..".into()));
    }
    Ok(())
}

/// Keywords: 1..=255 ASCII graphic characters excluding `( ) { % * " \ ]` (IMAP atom-ish),
/// returned lowercased.
pub fn normalize_keyword(kw: &str) -> Result<String> {
    if kw.is_empty() || kw.len() > MAX_KEYWORD_BYTES {
        return Err(Error::Invalid("keyword must be 1..=255 bytes".into()));
    }
    let ok = kw.bytes().all(|b| {
        b.is_ascii_graphic() && !matches!(b, b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']')
    });
    if !ok {
        return Err(Error::Invalid(
            "keyword contains a forbidden character".into(),
        ));
    }
    Ok(kw.to_ascii_lowercase())
}
// ---------- small helpers ----------

pub(crate) fn to_uid(v: i64) -> Result<u32> {
    match u32::try_from(v) {
        Ok(0) => Err(Error::storage("corrupt uid")),
        Ok(u) => Ok(u),
        Err(_) if v > 0 => Err(Error::Quota("folder uid space exhausted".into())),
        Err(_) => Err(Error::storage("corrupt uid")),
    }
}

pub(crate) fn to_flags(v: i64) -> Result<Flags> {
    u8::try_from(v)
        .ok()
        .and_then(Flags::from_bits)
        .ok_or_else(|| Error::storage("corrupt flags"))
}

/// The ids of a batch call: `1..=MAX_BATCH` given, duplicates dropped (first occurrence
/// wins, so input order is kept).
fn batch_ids(ids: &[MessageId]) -> Result<Vec<i64>> {
    if ids.is_empty() || ids.len() > MAX_BATCH {
        return Err(Error::Invalid(
            "a batch holds between 1 and 1000 items".into(),
        ));
    }
    let mut seen = HashSet::with_capacity(ids.len());
    Ok(ids
        .iter()
        .map(|i| i.0)
        .filter(|i| seen.insert(*i))
        .collect())
}

// ---------- the facade ----------

impl Mailbox {
    /// Open the mailbox of `account` over a repository (`Store::repos().mailbox`; `NotFound` if it has none).
    pub async fn open_with(
        repo: Arc<dyn MailboxRepo>,
        blobs: Arc<dyn BlobStore>,
        account: AccountId,
    ) -> Result<Mailbox> {
        let id = repo.mailbox_for_account(account).await?;
        Ok(Mailbox {
            repo,
            blobs,
            id,
            limits: MailboxLimits::default(),
            notifier: None,
        })
    }

    /// Announce every committed write to `notifier` (see [`ChangeNotifier`]).
    pub fn with_notifier(mut self, notifier: Arc<dyn ChangeNotifier>) -> Mailbox {
        self.notifier = Some(notifier);
        self
    }

    /// Unwrap a committed write, announcing its last change.
    fn note<T>(&self, c: crate::repo::Committed<T>) -> T {
        if let (Some(n), Some(seq)) = (&self.notifier, c.committed_seq) {
            n.changed(self.id, seq);
        }
        c.value
    }

    pub fn with_limits(mut self, limits: MailboxLimits) -> Mailbox {
        self.limits = limits;
        self
    }

    pub fn id(&self) -> MailboxId {
        self.id
    }

    pub fn limits(&self) -> MailboxLimits {
        self.limits
    }

    // ----- folders -----

    pub async fn create_folder(&self, parent: Option<FolderId>, name: &str) -> Result<FolderId> {
        validate_folder_name(name)?;
        Ok(self.note(self.repo.create_folder(self.id, parent, name).await?))
    }

    /// Rename a custom folder (system folders are `Invalid`).
    pub async fn rename_folder(&self, id: FolderId, name: &str) -> Result<()> {
        validate_folder_name(name)?;
        self.note(self.repo.rename_folder(self.id, id, name).await?);
        Ok(())
    }

    /// Re-parent a custom folder. Moving into itself or a descendant is `Conflict`; a result
    /// deeper than `MAX_FOLDER_DEPTH` is `Invalid`; system folders are `Invalid`.
    pub async fn move_folder(&self, id: FolderId, new_parent: Option<FolderId>) -> Result<()> {
        self.note(self.repo.move_folder(self.id, id, new_parent).await?);
        Ok(())
    }

    /// Delete an empty custom folder. Children or live messages are `Conflict`; system
    /// folders are `Invalid`. Rows of messages already expunged from it are purged (their
    /// blobs drop to refcount 0 for the cleanup job).
    pub async fn delete_folder(&self, id: FolderId) -> Result<()> {
        self.note(self.repo.delete_folder(self.id, id).await?);
        Ok(())
    }

    /// Live and unseen message counts of `folder` (IMAP STATUS); `NotFound` for a folder of
    /// another mailbox.
    pub async fn folder_counts(&self, folder: FolderId) -> Result<FolderCounts> {
        self.repo.folder_counts(self.id, folder).await
    }

    /// Set the IMAP subscription state of a folder (idempotent). System folders may be
    /// unsubscribed too: it is the client's list, not a property of the folder.
    pub async fn set_subscribed(&self, folder: FolderId, subscribed: bool) -> Result<()> {
        self.note(self.repo.set_subscribed(self.id, folder, subscribed).await?);
        Ok(())
    }

    /// All folders of the mailbox in id order.
    pub async fn list_folders(&self) -> Result<Vec<FolderInfo>> {
        self.repo.list_folders(self.id).await
    }

    // ----- messages -----

    /// Store a raw message in `folder`. Empty input is `Invalid` and input over
    /// `max_message_bytes` is `Quota`, both before anything is written. The folder is
    /// checked before the blob write (and again inside the transaction).
    ///
    /// Not idempotent: every call creates a message. Use [`Mailbox::deliver`] for retried
    /// deliveries.
    pub async fn append(&self, folder: FolderId, raw: Bytes, flags: Flags) -> Result<MessageRef> {
        match self.store_message(folder, raw, flags, None, None).await? {
            DeliverOutcome::Created(r) | DeliverOutcome::Duplicate(r) => Ok(r),
        }
    }

    /// [`Mailbox::append`] with the internal date the client gave (IMAP APPEND), unix seconds in
    /// `0..=MAX_SUPPLIED_TIME` (else `Invalid`, nothing stored).
    pub async fn append_dated(
        &self,
        folder: FolderId,
        raw: Bytes,
        flags: Flags,
        internal_date: i64,
    ) -> Result<MessageRef> {
        check_supplied_time(internal_date)?;
        let parsed = self.parse_raw(&raw)?;
        self.repo.folder_exists(self.id, folder).await?;
        let mut msg = self.put_message(folder, raw, flags, parsed).await?;
        msg.internal_date = Some(internal_date);
        match self.note(self.repo.append(self.id, &msg, None, None).await?) {
            DeliverOutcome::Created(r) | DeliverOutcome::Duplicate(r) => Ok(r),
        }
    }

    /// Idempotent [`Mailbox::append`]: the first delivery of `delivery_id` creates the
    /// message and a `deliveries` row in one transaction; a retry with the same id and the
    /// same content returns `Duplicate` with the ORIGINAL ref and writes nothing (no
    /// message, uid, or change). The same id with different content is
    /// `Conflict("delivery id reused with different content")`. A duplicate is detected by
    /// the id alone, so the retry's folder and flags are ignored. Validation (empty, size,
    /// folder) happens first, as in `append`. The row's `created_at` is the database clock.
    ///
    /// A retry rewrites its blob before the lookup (a refresh when the content exists); a
    /// conflicting retry therefore leaves an orphan blob file for the cleanup job.
    pub async fn deliver(
        &self,
        folder: FolderId,
        raw: Bytes,
        flags: Flags,
        delivery_id: DeliveryId,
    ) -> Result<DeliverOutcome> {
        self.store_message(
            folder,
            raw,
            flags,
            Some(Delivery {
                id: delivery_id,
                created_at: None,
            }),
            None,
        )
        .await
    }

    /// [`Mailbox::deliver`] that also fences and completes `job` in the same transaction (the
    /// `JobQueue::complete_in` pattern without a transaction handle): a stale epoch or an
    /// expired lease is `Conflict` and nothing is written.
    pub async fn deliver_fenced(
        &self,
        folder: FolderId,
        raw: Bytes,
        flags: Flags,
        delivery_id: DeliveryId,
        job: JobFence,
    ) -> Result<DeliverOutcome> {
        self.store_message(
            folder,
            raw,
            flags,
            Some(Delivery {
                id: delivery_id,
                created_at: None,
            }),
            Some(job),
        )
        .await
    }

    /// [`Mailbox::deliver`] with an explicit `created_at` / `internal_date` (unix seconds, stored
    /// verbatim), for tests. `now` must lie in `0..=MAX_SUPPLIED_TIME`, else `Error::Invalid`.
    #[doc(hidden)]
    pub async fn deliver_at(
        &self,
        folder: FolderId,
        raw: Bytes,
        flags: Flags,
        delivery_id: DeliveryId,
        now: i64,
    ) -> Result<DeliverOutcome> {
        check_supplied_time(now)?;
        self.store_message(
            folder,
            raw,
            flags,
            Some(Delivery {
                id: delivery_id,
                created_at: Some(now),
            }),
            None,
        )
        .await
    }

    /// Validate and parse one raw message and put its blob (blob first, metadata transaction
    /// second: a failed or retried transaction leaves at worst an orphan blob for the cleanup
    /// job, and a retry never repeats a blob write).
    fn parse_raw(&self, raw: &Bytes) -> Result<(i64, Envelope)> {
        if raw.is_empty() {
            return Err(Error::Invalid("empty message".into()));
        }
        if raw.len() > self.limits.max_message_bytes {
            return Err(Error::Quota("message exceeds the size limit".into()));
        }
        let size =
            i64::try_from(raw.len()).map_err(|_| Error::Quota("message too large".into()))?;
        Ok((size, rusty_mime::parse_envelope(raw)?))
    }

    async fn put_message(
        &self,
        folder: FolderId,
        raw: Bytes,
        flags: Flags,
        parsed: (i64, Envelope),
    ) -> Result<NewMessage> {
        let hash = self.blobs.put(raw).await?;
        Ok(NewMessage {
            folder,
            hash,
            size: parsed.0,
            flags,
            env: parsed.1,
            internal_date: None,
        })
    }

    /// Shared body of `append` and `deliver`.
    async fn store_message(
        &self,
        folder: FolderId,
        raw: Bytes,
        flags: Flags,
        delivery: Option<Delivery>,
        fence: Option<JobFence>,
    ) -> Result<DeliverOutcome> {
        let parsed = self.parse_raw(&raw)?;
        self.repo.folder_exists(self.id, folder).await?;
        let msg = self.put_message(folder, raw, flags, parsed).await?;
        Ok(self.note(self.repo.append(self.id, &msg, delivery.as_ref(), fence).await?))
    }

    /// Append every message of `raws` to `folder` in ONE transaction: consecutive ids, uids
    /// and change seqs in input order, all or nothing. `1..=MAX_BATCH` items.
    pub async fn append_many(
        &self,
        folder: FolderId,
        raws: Vec<Bytes>,
        flags: Flags,
    ) -> Result<Vec<MessageRef>> {
        if raws.is_empty() || raws.len() > MAX_BATCH {
            return Err(Error::Invalid(
                "a batch holds between 1 and 1000 items".into(),
            ));
        }
        self.repo.folder_exists(self.id, folder).await?;
        let mut msgs = Vec::with_capacity(raws.len());
        // Validate everything before the first blob is written.
        let parsed = raws
            .iter()
            .map(|r| self.parse_raw(r))
            .collect::<Result<Vec<_>>>()?;
        for (raw, p) in raws.into_iter().zip(parsed) {
            msgs.push(self.put_message(folder, raw, flags, p).await?);
        }
        Ok(self.note(self.repo.append_many(self.id, &msgs).await?))
    }

    /// Replace the standard flags of a live message.
    pub async fn set_flags(&self, id: MessageId, flags: Flags) -> Result<()> {
        self.note(self.repo.set_flags(self.id, id, flags).await?);
        Ok(())
    }

    /// Add a keyword (lowercased). Idempotent; a 33rd distinct keyword is `Quota`.
    pub async fn add_keyword(&self, id: MessageId, keyword: &str) -> Result<()> {
        let kw = normalize_keyword(keyword)?;
        self.note(self.repo.add_keyword(self.id, id, &kw).await?);
        Ok(())
    }

    /// Remove a keyword (case-insensitive). Removing an absent keyword is a no-op.
    pub async fn remove_keyword(&self, id: MessageId, keyword: &str) -> Result<()> {
        let kw = normalize_keyword(keyword)?;
        self.note(self.repo.remove_keyword(self.id, id, &kw).await?);
        Ok(())
    }

    /// Move a live message to `to`, giving it a fresh uid there. Moving to the folder it is
    /// already in changes nothing and returns the current ref.
    pub async fn move_message(&self, id: MessageId, to: FolderId) -> Result<MessageRef> {
        Ok(self.note(self.repo.move_message(self.id, id, to).await?))
    }

    /// Soft-delete a live message (sets `deleted_at`); the blob is untouched.
    pub async fn expunge(&self, id: MessageId) -> Result<()> {
        self.note(self.repo.expunge(self.id, id).await?);
        Ok(())
    }

    // ----- batches -----
    //
    // One call is ONE write transaction (mailbox row locked first), all or nothing. Counters
    // advance by exactly the number applied, each allocated with one `UPDATE ... RETURNING`; one
    // change_log row per applied message, in input order. Ids that do not exist, are
    // soft-deleted or belong to another mailbox are reported as `missing` and never touched.
    // Duplicate ids apply once. An empty input or more than `MAX_BATCH` items is `Invalid`. The
    // single-item methods above stay as they are; the equivalence tests (tests/batch.rs) prove
    // each batch equals the loop of singles.

    /// [`Mailbox::set_flags`] for many messages.
    pub async fn set_flags_many(&self, ids: &[MessageId], flags: Flags) -> Result<BatchOutcome> {
        let ids = batch_ids(ids)?;
        Ok(self.note(self.repo.set_flags_many(self.id, &ids, flags).await?))
    }

    /// Delta flag update for many messages (IMAP STORE +FLAGS / -FLAGS): applied to the live flags in
    /// one transaction, never overwriting what another session changed.
    pub async fn update_flags_many(&self, ids: &[MessageId], add: Flags, remove: Flags) -> Result<FlagUpdate> {
        let ids = batch_ids(ids)?;
        Ok(self.note(self.repo.update_flags_many(self.id, &ids, add, remove).await?))
    }

    /// Expunge the messages among `ids` that are STILL flagged Deleted (IMAP EXPUNGE/CLOSE with a
    /// possibly stale snapshot); the rest are reported as missing.
    pub async fn expunge_deleted_many(&self, ids: &[MessageId]) -> Result<BatchOutcome> {
        let ids = batch_ids(ids)?;
        Ok(self.note(self.repo.expunge_deleted_many(self.id, &ids).await?))
    }

    /// [`Mailbox::expunge`] for many messages.
    pub async fn expunge_many(&self, ids: &[MessageId]) -> Result<BatchOutcome> {
        let ids = batch_ids(ids)?;
        Ok(self.note(self.repo.expunge_many(self.id, &ids, None).await?))
    }

    /// [`Mailbox::expunge_many`] with an explicit `deleted_at` (unix seconds), for tests.
    #[doc(hidden)]
    pub async fn expunge_many_at(&self, ids: &[MessageId], now: i64) -> Result<BatchOutcome> {
        check_supplied_time(now)?;
        let ids = batch_ids(ids)?;
        Ok(self.note(self.repo.expunge_many(self.id, &ids, Some(now)).await?))
    }

    /// Copy live messages into `to` (IMAP COPY): each copy is a NEW message with a fresh id and
    /// consecutive uids in input order, sharing the blob and carrying flags, keywords, internal
    /// date and thread of its source. Duplicate ids copy once; ids that are not live messages of
    /// this mailbox are `missing`; an unknown destination is `NotFound` even if every id is
    /// missing. One transaction, all or nothing.
    pub async fn copy_many(&self, ids: &[MessageId], to: FolderId) -> Result<CopyBatch> {
        let ids = batch_ids(ids)?;
        Ok(self.note(self.repo.copy_many(self.id, &ids, to).await?))
    }

    /// [`Mailbox::move_message`] for many messages: the moved ones get contiguous fresh uids
    /// in `to`, in input order. A message already in `to` keeps its uid and logs nothing. An
    /// unknown destination folder is `NotFound` even if every id is missing.
    pub async fn move_many(&self, ids: &[MessageId], to: FolderId) -> Result<MoveBatch> {
        let ids = batch_ids(ids)?;
        Ok(self.note(self.repo.move_many(self.id, &ids, to).await?))
    }

    /// [`Mailbox::deliver`] for many messages into `folder`: at most [`MAX_BATCH`] items and
    /// [`MAX_DELIVER_BATCH_BYTES`] raw bytes in total (`Invalid` / `Quota`), each also within
    /// the per-message limit. The outcomes are in input order and equal what calling
    /// `deliver` for each item in order would return, including a repeated delivery id
    /// inside the batch (the second is `Duplicate`, or `Conflict` if its content differs).
    ///
    /// Everything is validated before anything is written. The blobs are then written first
    /// (idempotent), then ONE transaction does all the metadata. If it fails (for example a
    /// reused delivery id with different content, which is `Conflict` for the whole batch),
    /// nothing is applied and the blobs are orphans for the cleanup job.
    pub async fn deliver_many(
        &self,
        folder: FolderId,
        items: Vec<DeliverItem>,
    ) -> Result<Vec<DeliverOutcome>> {
        self.deliver_many_inner(folder, items, None, None).await
    }

    /// [`Mailbox::deliver_many`] that also fences and completes `job` in the same transaction.
    pub async fn deliver_many_fenced(
        &self,
        folder: FolderId,
        items: Vec<DeliverItem>,
        job: JobFence,
    ) -> Result<Vec<DeliverOutcome>> {
        self.deliver_many_inner(folder, items, None, Some(job)).await
    }

    /// [`Mailbox::deliver_many`] with an explicit `internal_date` / `created_at` (unix
    /// seconds, applied to every item), for tests.
    #[doc(hidden)]
    pub async fn deliver_many_at(
        &self,
        folder: FolderId,
        items: Vec<DeliverItem>,
        now: i64,
    ) -> Result<Vec<DeliverOutcome>> {
        check_supplied_time(now)?;
        self.deliver_many_inner(folder, items, Some(now), None).await
    }

    async fn deliver_many_inner(
        &self,
        folder: FolderId,
        items: Vec<DeliverItem>,
        at: Option<i64>,
        fence: Option<JobFence>,
    ) -> Result<Vec<DeliverOutcome>> {
        if items.is_empty() || items.len() > MAX_BATCH {
            return Err(Error::Invalid(
                "a batch holds between 1 and 1000 items".into(),
            ));
        }
        let mut total = 0usize;
        let mut envs = Vec::with_capacity(items.len());
        for it in &items {
            if it.raw.is_empty() {
                return Err(Error::Invalid("empty message".into()));
            }
            if it.raw.len() > self.limits.max_message_bytes {
                return Err(Error::Quota("message exceeds the size limit".into()));
            }
            total = total.saturating_add(it.raw.len());
            if total > MAX_DELIVER_BATCH_BYTES {
                return Err(Error::Quota("batch exceeds the total size limit".into()));
            }
            envs.push(rusty_mime::parse_envelope(&it.raw)?);
        }
        self.repo.folder_exists(self.id, folder).await?;
        let mut entries = Vec::with_capacity(items.len());
        for (it, env) in items.into_iter().zip(envs) {
            let size = i64::try_from(it.raw.len())
                .map_err(|_| Error::Quota("message too large".into()))?;
            let hash = self.blobs.put(it.raw).await?;
            entries.push(DeliverEntry {
                msg: NewMessage {
                    folder,
                    hash,
                    size,
                    flags: it.flags,
                    env,
                    internal_date: None,
                },
                delivery_id: it.delivery_id,
            });
        }
        Ok(self.note(self.repo.deliver_many(self.id, &entries, at, fence).await?))
    }

    /// Hard-delete the soft-deleted messages of THIS mailbox with `deleted_at < older_than`
    /// (unix seconds), one bounded transaction at a time ([`PURGE_BATCH`] messages), until none
    /// are left. A message that still has a `deliveries` row is kept (see
    /// [`purge_deleted`](crate::purge_deleted), which does the same for every mailbox of a
    /// SQLite store). Returns how many were removed.
    pub async fn purge_deleted(&self, older_than: i64) -> Result<u64> {
        let mut total = 0u64;
        loop {
            let n = self
                .repo
                .purge_deleted(self.id, older_than, PURGE_BATCH)
                .await?
                .value;
            total += n;
            if n < PURGE_BATCH as u64 {
                return Ok(total);
            }
        }
    }

    // ----- reads -----

    /// One message, soft-deleted ones included (`MessageDetail::deleted`). `NotFound` for an id
    /// of another mailbox.
    pub async fn get_message(&self, id: MessageId) -> Result<MessageDetail> {
        self.repo.get_message(self.id, id).await
    }

    /// Where the raw bytes of a live message are (blob hash and size).
    pub async fn get_raw_ref(&self, id: MessageId) -> Result<RawRef> {
        self.repo.get_raw_ref(self.id, id).await
    }

    /// The raw bytes of a live message (`NotFound` for an id of another mailbox or a deleted
    /// message). The size is the stored size; the blob is read in one piece, so callers bound
    /// what they ask for by the mailbox size limit.
    pub async fn get_raw(&self, id: MessageId) -> Result<Bytes> {
        let r = self.repo.get_raw_ref(self.id, id).await?;
        self.blobs.get(&r.blob).await
    }

    /// Keywords of the live messages among `ids` in one call (`1..=MAX_BATCH` ids, else `Invalid`);
    /// messages without keywords are absent from the map.
    pub async fn keywords_many(
        &self,
        ids: &[MessageId],
    ) -> Result<std::collections::HashMap<MessageId, Vec<String>>> {
        let ids = batch_ids(ids)?;
        let mut out: std::collections::HashMap<MessageId, Vec<String>> = std::collections::HashMap::new();
        for (id, kw) in self.repo.keywords_many(self.id, &ids).await? {
            out.entry(MessageId(id)).or_default().push(kw);
        }
        Ok(out)
    }

    /// The change seq of the last change of this mailbox (`0` before the first). A reader that
    /// takes this number BEFORE it reads a snapshot can then follow `changes_since(seq)` without
    /// missing anything.
    pub async fn latest_change_seq(&self) -> Result<i64> {
        self.repo.latest_change_seq(self.id).await
    }

    /// The keywords of a live message, sorted.
    pub async fn keywords(&self, id: MessageId) -> Result<Vec<String>> {
        self.repo.keywords(self.id, id).await
    }

    /// One page of the live messages of `folder` with `uid > after_uid`, ordered by uid: at
    /// most `limit` entries, clamped to [`MAX_PAGE`]; `limit == 0` is `Invalid`. Start with
    /// `after_uid = 0` and continue from the last uid returned; an empty page is the end.
    pub async fn list_folder(
        &self,
        folder: FolderId,
        after_uid: u32,
        limit: u32,
    ) -> Result<Vec<MessageSummary>> {
        let limit = page_limit(limit)?;
        self.repo.list_folder(self.id, folder, after_uid, limit).await
    }

    /// One page of change-log entries with `seq > seq`, ordered by seq: at most `limit`
    /// entries, clamped to [`MAX_PAGE`]; `limit == 0` is `Invalid`. `more` is true if entries
    /// follow the last one returned; continue from its `seq`.
    pub async fn changes_since(&self, seq: i64, limit: u32) -> Result<ChangePage> {
        let limit = page_limit(limit)?;
        self.repo.changes_since(self.id, seq, limit).await
    }
}
