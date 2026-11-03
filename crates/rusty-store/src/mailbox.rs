//! `Mailbox`: the high-level facade over one account's folders, messages and change log.
//!
//! Invariants:
//! - Rule: every Mailbox write transaction starts with `BEGIN IMMEDIATE` and touches the
//!   mailbox row first (`Mailbox::begin` reads `mailboxes` by id before anything else; a
//!   missing row is `NotFound`). With SQLite the write lock is already global; the fixed
//!   order is the lock-ordering rule a row-locking backend (Postgres: `SELECT ... FOR
//!   UPDATE` on the mailbox row) relies on to serialize writers of one mailbox without
//!   deadlocks.
//! - The counters `mailboxes.next_msg_id`, `mailboxes.next_change_seq`,
//!   `mailboxes.next_folder_id` and `folders.next_uid` are read and incremented inside that
//!   transaction, so message ids, uids and change seqs are gapless and monotonic, and folder
//!   ids are never reused (a deleted folder's id is not handed out again).
//! - Reads are paginated: `list_folder` and `changes_since` return at most [`MAX_PAGE`]
//!   entries per call (a larger `limit` is clamped, `0` is `Invalid`).
//! - Every lookup is scoped by `mailbox_id`: ids of another mailbox are `NotFound`.
//! - `deliver` is `append` plus a `deliveries` row in the same transaction (see
//!   `store_message`); the duplicate lookup runs after `begin`, so it is race-free.
//! - `append` writes the blob before the metadata transaction; if the transaction fails the
//!   blob is an orphan for the cleanup job (spec §5).
//! - Nothing here logs message content; error texts never echo caller input.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use rusty_core::{
    AccountId, DeliveryId, Error, Flags, FolderId, MailboxId, MessageId, Result, ThreadId,
};
use rusty_mime::Envelope;
use sqlx::{Sqlite, SqliteConnection, Transaction};

use crate::{map_db_error, BlobStore, SqliteStore};

pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_KEYWORDS_PER_MESSAGE: usize = 32;
pub const MAX_KEYWORD_BYTES: usize = 255;
pub const MAX_FOLDER_NAME_BYTES: usize = 255;
/// Maximum number of levels in the folder tree (a root folder is level 1).
pub const MAX_FOLDER_DEPTH: usize = 64;
/// Bound on parent lookups per appended message (hostile `References` headers).
const MAX_THREAD_CANDIDATES: usize = 32;
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
    fn to_row(self) -> (&'static str, i64) {
        match self {
            ChangeItem::Message(id) => ("message", id.0),
            ChangeItem::Folder(id) => ("folder", id.0),
        }
    }

    fn from_row(kind: &str, item_id: i64) -> Result<ChangeItem> {
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
    fn as_str(self) -> &'static str {
        match self {
            ChangeOp::Create => "create",
            ChangeOp::Update => "update",
            ChangeOp::Destroy => "destroy",
        }
    }

    fn parse(s: &str) -> Result<ChangeOp> {
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
fn page_limit(limit: u32) -> Result<i64> {
    if limit == 0 {
        return Err(Error::Invalid("page limit must be at least 1".into()));
    }
    Ok(i64::from(limit.min(MAX_PAGE)))
}

/// One account's mailbox. Cheap to clone; clones share the pool and blob store.
///
/// Invariant: every code path that inserts a row into `mailboxes` must also seed
/// `next_folder_id` (migration 0003 column; `take_folder_id` hands out folder ids from it),
/// or `create_folder` would produce colliding or missing ids.
#[derive(Clone)]
pub struct Mailbox {
    store: SqliteStore,
    blobs: Arc<dyn BlobStore>,
    id: MailboxId,
    limits: MailboxLimits,
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

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Issue a new folder UIDVALIDITY inside the caller's transaction.
///
/// RFC 3501/9051 §2.3.1.1: a folder recreated under a reused name (and here also a reused
/// folder id) must get a strictly greater UIDVALIDITY. `mailboxes.uid_validity` is the
/// persisted high-water mark; the new value is `max(now_secs, high_water + 1)` and the mark
/// is advanced to it in the same transaction, in one statement. Past `u32::MAX` it is
/// `Quota`: the caller returns the error, its transaction rolls back, and the mark is
/// unchanged.
async fn take_uid_validity(conn: &mut SqliteConnection, mb: MailboxId) -> Result<i64> {
    let v: i64 = sqlx::query_scalar(
        "UPDATE mailboxes SET uid_validity = max(?1, uid_validity + 1) WHERE id = ?2 \
         RETURNING uid_validity",
    )
    .bind(now())
    .bind(mb.0)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| Error::NotFound("mailbox".into()))?;
    if v > i64::from(u32::MAX) {
        return Err(Error::Quota("folder UIDVALIDITY space exhausted".into()));
    }
    Ok(v)
}

/// Take the next folder id of the mailbox (`mailboxes.next_folder_id`, migration 0003).
/// Every code path that inserts a `mailboxes` row must seed `next_folder_id`.
async fn take_folder_id(conn: &mut SqliteConnection, mb: MailboxId) -> Result<i64> {
    sqlx::query_scalar(
        "UPDATE mailboxes SET next_folder_id = next_folder_id + 1 WHERE id = ? \
         RETURNING next_folder_id - 1",
    )
    .bind(mb.0)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| Error::NotFound("mailbox".into()))
}

fn to_uid(v: i64) -> Result<u32> {
    match u32::try_from(v) {
        Ok(0) => Err(Error::storage("corrupt uid")),
        Ok(u) => Ok(u),
        Err(_) if v > 0 => Err(Error::Quota("folder uid space exhausted".into())),
        Err(_) => Err(Error::storage("corrupt uid")),
    }
}

fn to_flags(v: i64) -> Result<Flags> {
    u8::try_from(v)
        .ok()
        .and_then(Flags::from_bits)
        .ok_or_else(|| Error::storage("corrupt flags"))
}

fn not_found_folder() -> Error {
    Error::NotFound("folder".into())
}

fn not_found_message() -> Error {
    Error::NotFound("message".into())
}

// ---------- in-transaction primitives ----------

/// Append a change_log row with the next seq of the mailbox.
async fn log_change(
    conn: &mut SqliteConnection,
    mb: MailboxId,
    item: ChangeItem,
    op: ChangeOp,
) -> Result<i64> {
    let (kind, item_id) = item.to_row();
    let seq: i64 = sqlx::query_scalar(
        "UPDATE mailboxes SET next_change_seq = next_change_seq + 1 WHERE id = ? \
         RETURNING next_change_seq - 1",
    )
    .bind(mb.0)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| Error::NotFound("mailbox".into()))?;
    sqlx::query(
        "INSERT INTO change_log (mailbox_id, seq, kind, item_id, op) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(mb.0)
    .bind(seq)
    .bind(kind)
    .bind(item_id)
    .bind(op.as_str())
    .execute(&mut *conn)
    .await
    .map_err(map_db_error)?;
    Ok(seq)
}

/// Take the next uid of `folder` (NotFound if the folder is not in this mailbox).
async fn take_uid(conn: &mut SqliteConnection, mb: MailboxId, folder: FolderId) -> Result<u32> {
    let uid: i64 = sqlx::query_scalar(
        "UPDATE folders SET next_uid = next_uid + 1 WHERE mailbox_id = ? AND id = ? \
         RETURNING next_uid - 1",
    )
    .bind(mb.0)
    .bind(folder.0)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_db_error)?
    .ok_or_else(not_found_folder)?;
    to_uid(uid)
}

async fn take_msg_id(conn: &mut SqliteConnection, mb: MailboxId) -> Result<i64> {
    sqlx::query_scalar(
        "UPDATE mailboxes SET next_msg_id = next_msg_id + 1 WHERE id = ? RETURNING next_msg_id - 1",
    )
    .bind(mb.0)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| Error::NotFound("mailbox".into()))
}

/// `(parent_id, role)` of a folder of this mailbox.
async fn folder_row(
    conn: &mut SqliteConnection,
    mb: MailboxId,
    id: FolderId,
) -> Result<(Option<i64>, String)> {
    sqlx::query_as("SELECT parent_id, role FROM folders WHERE mailbox_id = ? AND id = ?")
        .bind(mb.0)
        .bind(id.0)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_db_error)?
        .ok_or_else(not_found_folder)
}

/// `Invalid` for system folders (role other than `custom`).
async fn custom_folder(
    conn: &mut SqliteConnection,
    mb: MailboxId,
    id: FolderId,
) -> Result<Option<i64>> {
    let (parent, role) = folder_row(conn, mb, id).await?;
    if role != "custom" {
        return Err(Error::Invalid("system folders cannot be changed".into()));
    }
    Ok(parent)
}

/// Number of levels from `start` up to its root (start included). `Conflict` if `forbidden`
/// is `start` or one of its ancestors; `Invalid` if the chain exceeds `MAX_FOLDER_DEPTH`
/// (which also stops on corrupt, cyclic data).
async fn ancestor_depth(
    conn: &mut SqliteConnection,
    mb: MailboxId,
    start: FolderId,
    forbidden: Option<FolderId>,
) -> Result<usize> {
    // One recursive CTE walks up from `start`, bounded by MAX_FOLDER_DEPTH + 1 rows (the
    // extra row is what proves "too deep"; it also stops on cyclic data). The verdict is
    // then read off the rows in depth order, with the same checks in the same order as a
    // step-by-step walk. Portable SQL (WITH RECURSIVE, numbered parameters).
    let rows: Vec<(i64, Option<i64>)> = sqlx::query_as(
        "WITH RECURSIVE up(id, parent_id, depth) AS ( \
           SELECT id, parent_id, 1 FROM folders WHERE mailbox_id = ?1 AND id = ?2 \
           UNION ALL \
           SELECT f.id, f.parent_id, u.depth + 1 FROM folders f JOIN up u \
             ON f.mailbox_id = ?1 AND f.id = u.parent_id \
           WHERE u.depth <= ?3 \
         ) SELECT id, parent_id FROM up ORDER BY depth",
    )
    .bind(mb.0)
    .bind(start.0)
    .bind(MAX_FOLDER_DEPTH as i64)
    .fetch_all(&mut *conn)
    .await
    .map_err(map_db_error)?;
    let mut depth = 0usize;
    let mut parent = None;
    for (id, p) in rows {
        if forbidden.is_some_and(|f| f.0 == id) {
            return Err(Error::Conflict(
                "cannot move a folder into itself or a descendant".into(),
            ));
        }
        depth += 1;
        if depth > MAX_FOLDER_DEPTH {
            return Err(Error::Invalid("folder tree too deep".into()));
        }
        parent = p;
    }
    // No rows: `start` is not in this mailbox. A parent that is not there either (corrupt
    // data) ends the walk early with the same answer.
    if depth == 0 || parent.is_some() {
        return Err(not_found_folder());
    }
    Ok(depth)
}

/// Levels in the subtree rooted at `id` (a leaf is 1), capped just above the limit.
async fn subtree_height(conn: &mut SqliteConnection, mb: MailboxId, id: FolderId) -> Result<usize> {
    let h: i64 = sqlx::query_scalar(
        "WITH RECURSIVE sub(id, depth) AS ( \
           SELECT ?, 1 \
           UNION ALL \
           SELECT f.id, s.depth + 1 FROM folders f JOIN sub s \
             ON f.mailbox_id = ? AND f.parent_id = s.id \
           WHERE s.depth <= ? \
         ) SELECT MAX(depth) FROM sub",
    )
    .bind(id.0)
    .bind(mb.0)
    .bind(MAX_FOLDER_DEPTH as i64)
    .fetch_one(&mut *conn)
    .await
    .map_err(map_db_error)?;
    usize::try_from(h).map_err(|_| Error::storage("corrupt folder tree"))
}

/// Ensure a live (not soft-deleted) message of this mailbox exists; returns `(folder_id, uid)`.
async fn live_message(
    conn: &mut SqliteConnection,
    mb: MailboxId,
    id: MessageId,
) -> Result<(i64, i64)> {
    sqlx::query_as(
        "SELECT folder_id, uid FROM messages \
         WHERE mailbox_id = ? AND id = ? AND deleted_at IS NULL",
    )
    .bind(mb.0)
    .bind(id.0)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_db_error)?
    .ok_or_else(not_found_message)
}

/// Thread of the nearest known ancestor: the references from last to first, then
/// In-Reply-To. Soft-deleted messages still anchor their thread.
///
/// At most `MAX_THREAD_CANDIDATES` (32) ids are tried. With more than 32 References only
/// the last 32 are consulted and In-Reply-To is not consulted at all.
async fn find_thread(
    conn: &mut SqliteConnection,
    mb: MailboxId,
    env: &Envelope,
) -> Result<Option<i64>> {
    let candidates: Vec<&str> = env
        .references
        .iter()
        .rev()
        .chain(env.in_reply_to.iter())
        .take(MAX_THREAD_CANDIDATES)
        .map(String::as_str)
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    let known = lookup_threads(conn, mb, &candidates).await?;
    Ok(candidates.iter().find_map(|c| known.get(*c).copied()))
}

/// Thread of the oldest message (smallest id) for each of the given Message-IDs, in one
/// statement per `SQL_CHUNK` ids (one for the at most 32 thread candidates). Soft-deleted
/// messages count. Portable SQL: a grouped subquery and numbered parameters.
async fn lookup_threads(
    conn: &mut SqliteConnection,
    mb: MailboxId,
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
        let mut q = sqlx::query_as::<_, (String, i64)>(&sql).bind(mb.0);
        for h in chunk {
            q = q.bind(*h);
        }
        out.extend(q.fetch_all(&mut *conn).await.map_err(map_db_error)?);
    }
    Ok(out)
}

/// Most bound ids per statement in the set-based queries (SQLite's default limit is 32,766
/// variables, Postgres's 65,535; this stays far below both).
const SQL_CHUNK: usize = 500;

/// `?first,?first+1,..` for `n` numbered parameters. Only numbers are formatted into SQL text.
fn placeholders(first: usize, n: usize) -> String {
    (first..first + n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

async fn commit(tx: Transaction<'static, Sqlite>) -> Result<()> {
    tx.commit().await.map_err(map_db_error)
}

// ---------- the facade ----------

impl Mailbox {
    /// Open the mailbox of `account` (`NotFound` if it has none).
    pub async fn open(
        store: SqliteStore,
        blobs: Arc<dyn BlobStore>,
        account: AccountId,
    ) -> Result<Mailbox> {
        let id: i64 = sqlx::query_scalar("SELECT id FROM mailboxes WHERE account_id = ?")
            .bind(account.0)
            .fetch_optional(store.pool())
            .await
            .map_err(map_db_error)?
            .ok_or_else(|| Error::NotFound("mailbox for account".into()))?;
        Ok(Mailbox {
            store,
            blobs,
            id: MailboxId(id),
            limits: MailboxLimits::default(),
        })
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

    /// Start a write transaction (`BEGIN IMMEDIATE`) and touch the mailbox row first (see the
    /// module docs). `NotFound` if the mailbox no longer exists.
    async fn begin(&self) -> Result<Transaction<'static, Sqlite>> {
        let mut tx = self.store.begin_write().await?;
        sqlx::query_scalar::<_, i64>("SELECT id FROM mailboxes WHERE id = ?")
            .bind(self.id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_db_error)?
            .ok_or_else(|| Error::NotFound("mailbox".into()))?;
        Ok(tx)
    }

    // ----- folders -----

    pub async fn create_folder(&self, parent: Option<FolderId>, name: &str) -> Result<FolderId> {
        validate_folder_name(name)?;
        let mut tx = self.begin().await?;
        if let Some(p) = parent {
            let depth = ancestor_depth(&mut tx, self.id, p, None).await?;
            if depth + 1 > MAX_FOLDER_DEPTH {
                return Err(Error::Invalid("folder tree too deep".into()));
            }
        }
        let uid_validity = take_uid_validity(&mut tx, self.id).await?;
        let id = take_folder_id(&mut tx, self.id).await?;
        sqlx::query(
            "INSERT INTO folders (mailbox_id, id, parent_id, name, role, uid_validity) \
             VALUES (?, ?, ?, ?, 'custom', ?)",
        )
        .bind(self.id.0)
        .bind(id)
        .bind(parent.map(|p| p.0))
        .bind(name)
        .bind(uid_validity)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?;
        log_change(
            &mut tx,
            self.id,
            ChangeItem::Folder(FolderId(id)),
            ChangeOp::Create,
        )
        .await?;
        commit(tx).await?;
        Ok(FolderId(id))
    }

    /// Rename a custom folder (system folders are `Invalid`).
    pub async fn rename_folder(&self, id: FolderId, name: &str) -> Result<()> {
        validate_folder_name(name)?;
        let mut tx = self.begin().await?;
        custom_folder(&mut tx, self.id, id).await?;
        sqlx::query("UPDATE folders SET name = ? WHERE mailbox_id = ? AND id = ?")
            .bind(name)
            .bind(self.id.0)
            .bind(id.0)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        log_change(&mut tx, self.id, ChangeItem::Folder(id), ChangeOp::Update).await?;
        commit(tx).await
    }

    /// Re-parent a custom folder. Moving into itself or a descendant is `Conflict`; a result
    /// deeper than `MAX_FOLDER_DEPTH` is `Invalid`; system folders are `Invalid`.
    pub async fn move_folder(&self, id: FolderId, new_parent: Option<FolderId>) -> Result<()> {
        let mut tx = self.begin().await?;
        let old_parent = custom_folder(&mut tx, self.id, id).await?;
        if old_parent == new_parent.map(|p| p.0) {
            return Ok(());
        }
        if let Some(p) = new_parent {
            let depth = ancestor_depth(&mut tx, self.id, p, Some(id)).await?;
            let height = subtree_height(&mut tx, self.id, id).await?;
            if depth + height > MAX_FOLDER_DEPTH {
                return Err(Error::Invalid("folder tree too deep".into()));
            }
        }
        sqlx::query("UPDATE folders SET parent_id = ? WHERE mailbox_id = ? AND id = ?")
            .bind(new_parent.map(|p| p.0))
            .bind(self.id.0)
            .bind(id.0)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        log_change(&mut tx, self.id, ChangeItem::Folder(id), ChangeOp::Update).await?;
        commit(tx).await
    }

    /// Delete an empty custom folder. Children or live messages are `Conflict`; system
    /// folders are `Invalid`. Rows of messages already expunged from it are purged (their
    /// blobs drop to refcount 0 for the cleanup job).
    pub async fn delete_folder(&self, id: FolderId) -> Result<()> {
        let mut tx = self.begin().await?;
        custom_folder(&mut tx, self.id, id).await?;
        let children: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM folders WHERE mailbox_id = ? AND parent_id = ?",
        )
        .bind(self.id.0)
        .bind(id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_error)?;
        if children > 0 {
            return Err(Error::Conflict("folder has subfolders".into()));
        }
        let live: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages \
             WHERE mailbox_id = ? AND folder_id = ? AND deleted_at IS NULL",
        )
        .bind(self.id.0)
        .bind(id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_error)?;
        if live > 0 {
            return Err(Error::Conflict("folder still contains messages".into()));
        }
        sqlx::query(
            "DELETE FROM messages WHERE mailbox_id = ? AND folder_id = ? AND deleted_at IS NOT NULL",
        )
        .bind(self.id.0)
        .bind(id.0)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?;
        sqlx::query("DELETE FROM folders WHERE mailbox_id = ? AND id = ?")
            .bind(self.id.0)
            .bind(id.0)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        log_change(&mut tx, self.id, ChangeItem::Folder(id), ChangeOp::Destroy).await?;
        commit(tx).await
    }

    // ----- messages -----

    /// Store a raw message in `folder`. Empty input is `Invalid` and input over
    /// `max_message_bytes` is `Quota`, both before anything is written. The folder is
    /// checked before the blob write (and again inside the transaction).
    ///
    /// Not idempotent: every call creates a message. Use [`Mailbox::deliver`] for retried
    /// deliveries.
    pub async fn append(&self, folder: FolderId, raw: Bytes, flags: Flags) -> Result<MessageRef> {
        match self.store_message(folder, raw, flags, None).await? {
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
        self.store_message(folder, raw, flags, Some((delivery_id, None)))
            .await
    }

    /// [`Mailbox::deliver`] with an explicit `created_at` (unix seconds), for tests.
    pub async fn deliver_at(
        &self,
        folder: FolderId,
        raw: Bytes,
        flags: Flags,
        delivery_id: DeliveryId,
        now: i64,
    ) -> Result<DeliverOutcome> {
        self.store_message(folder, raw, flags, Some((delivery_id, Some(now))))
            .await
    }

    /// Shared body of `append` and `deliver`. `delivery` is the delivery id and an optional
    /// `created_at` override (`None`: database clock).
    async fn store_message(
        &self,
        folder: FolderId,
        raw: Bytes,
        flags: Flags,
        delivery: Option<(DeliveryId, Option<i64>)>,
    ) -> Result<DeliverOutcome> {
        if raw.is_empty() {
            return Err(Error::Invalid("empty message".into()));
        }
        if raw.len() > self.limits.max_message_bytes {
            return Err(Error::Quota("message exceeds the size limit".into()));
        }
        let size =
            i64::try_from(raw.len()).map_err(|_| Error::Quota("message too large".into()))?;
        let env = rusty_mime::parse_envelope(&raw)?;
        {
            let mut conn = self.store.pool().acquire().await.map_err(map_db_error)?;
            folder_row(&mut conn, self.id, folder).await?;
        }
        let hash = self.blobs.put(raw).await?;

        let mut tx = self.begin().await?;
        if let Some((did, _)) = delivery {
            // Inside BEGIN IMMEDIATE no other writer can insert between this lookup and
            // our insert below; the primary key is the backstop.
            let hit: Option<(i64, String)> = sqlx::query_as(
                "SELECT message_id, blob_hash FROM deliveries \
                 WHERE mailbox_id = ? AND delivery_id = ?",
            )
            .bind(self.id.0)
            .bind(&did.0[..])
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_db_error)?;
            if let Some((msg_id, blob_hash)) = hit {
                if blob_hash != hash.as_str() {
                    return Err(Error::Conflict(
                        "delivery id reused with different content".into(),
                    ));
                }
                let uid: i64 =
                    sqlx::query_scalar("SELECT uid FROM messages WHERE mailbox_id = ? AND id = ?")
                        .bind(self.id.0)
                        .bind(msg_id)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(map_db_error)?
                        .ok_or_else(|| Error::storage("delivery refers to a missing message"))?;
                return Ok(DeliverOutcome::Duplicate(MessageRef {
                    id: MessageId(msg_id),
                    uid: to_uid(uid)?,
                }));
            }
        }
        sqlx::query("INSERT INTO blobs (hash, size) VALUES (?, ?) ON CONFLICT (hash) DO NOTHING")
            .bind(hash.as_str())
            .bind(size)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        let uid = take_uid(&mut tx, self.id, folder).await?;
        let id = take_msg_id(&mut tx, self.id).await?;
        let thread_id = find_thread(&mut tx, self.id, &env).await?.unwrap_or(id);
        sqlx::query(
            "INSERT INTO messages (mailbox_id, id, folder_id, uid, blob_hash, size, internal_date, \
             subject, from_addr, to_addrs, message_id_hdr, thread_id, flags) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(self.id.0)
        .bind(id)
        .bind(folder.0)
        .bind(i64::from(uid))
        .bind(hash.as_str())
        .bind(size)
        .bind(now())
        .bind(&env.subject)
        .bind(&env.from)
        .bind(&env.to)
        .bind(env.message_id.as_deref())
        .bind(thread_id)
        .bind(i64::from(flags.bits()))
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?;
        if let Some((did, at)) = delivery {
            sqlx::query(
                "INSERT INTO deliveries (mailbox_id, delivery_id, message_id, blob_hash, created_at) \
                 VALUES (?, ?, ?, ?, COALESCE(?, CAST(strftime('%s', 'now') AS INTEGER)))",
            )
            .bind(self.id.0)
            .bind(&did.0[..])
            .bind(id)
            .bind(hash.as_str())
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        }
        log_change(
            &mut tx,
            self.id,
            ChangeItem::Message(MessageId(id)),
            ChangeOp::Create,
        )
        .await?;
        commit(tx).await?;
        Ok(DeliverOutcome::Created(MessageRef {
            id: MessageId(id),
            uid,
        }))
    }

    /// Replace the standard flags of a live message.
    pub async fn set_flags(&self, id: MessageId, flags: Flags) -> Result<()> {
        let mut tx = self.begin().await?;
        let n = sqlx::query(
            "UPDATE messages SET flags = ? WHERE mailbox_id = ? AND id = ? AND deleted_at IS NULL",
        )
        .bind(i64::from(flags.bits()))
        .bind(self.id.0)
        .bind(id.0)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?
        .rows_affected();
        if n == 0 {
            return Err(not_found_message());
        }
        log_change(&mut tx, self.id, ChangeItem::Message(id), ChangeOp::Update).await?;
        commit(tx).await
    }

    /// Add a keyword (lowercased). Idempotent; a 33rd distinct keyword is `Quota`.
    pub async fn add_keyword(&self, id: MessageId, keyword: &str) -> Result<()> {
        let kw = normalize_keyword(keyword)?;
        let mut tx = self.begin().await?;
        live_message(&mut tx, self.id, id).await?;
        let present: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM keywords WHERE mailbox_id = ? AND message_id = ? AND keyword = ?",
        )
        .bind(self.id.0)
        .bind(id.0)
        .bind(&kw)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db_error)?;
        if present.is_some() {
            return Ok(());
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM keywords WHERE mailbox_id = ? AND message_id = ?",
        )
        .bind(self.id.0)
        .bind(id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_error)?;
        if usize::try_from(count).map_or(true, |c| c >= MAX_KEYWORDS_PER_MESSAGE) {
            return Err(Error::Quota("too many keywords on message".into()));
        }
        sqlx::query("INSERT INTO keywords (mailbox_id, message_id, keyword) VALUES (?, ?, ?)")
            .bind(self.id.0)
            .bind(id.0)
            .bind(&kw)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        log_change(&mut tx, self.id, ChangeItem::Message(id), ChangeOp::Update).await?;
        commit(tx).await
    }

    /// Remove a keyword (case-insensitive). Removing an absent keyword is a no-op.
    pub async fn remove_keyword(&self, id: MessageId, keyword: &str) -> Result<()> {
        let kw = normalize_keyword(keyword)?;
        let mut tx = self.begin().await?;
        live_message(&mut tx, self.id, id).await?;
        let n = sqlx::query(
            "DELETE FROM keywords WHERE mailbox_id = ? AND message_id = ? AND keyword = ?",
        )
        .bind(self.id.0)
        .bind(id.0)
        .bind(&kw)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?
        .rows_affected();
        if n == 0 {
            return Ok(());
        }
        log_change(&mut tx, self.id, ChangeItem::Message(id), ChangeOp::Update).await?;
        commit(tx).await
    }

    /// Move a live message to `to`, giving it a fresh uid there. Moving to the folder it is
    /// already in changes nothing and returns the current ref.
    pub async fn move_message(&self, id: MessageId, to: FolderId) -> Result<MessageRef> {
        let mut tx = self.begin().await?;
        let (folder, uid) = live_message(&mut tx, self.id, id).await?;
        if folder == to.0 {
            return Ok(MessageRef {
                id,
                uid: to_uid(uid)?,
            });
        }
        let new_uid = take_uid(&mut tx, self.id, to).await?;
        sqlx::query("UPDATE messages SET folder_id = ?, uid = ? WHERE mailbox_id = ? AND id = ?")
            .bind(to.0)
            .bind(i64::from(new_uid))
            .bind(self.id.0)
            .bind(id.0)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        log_change(&mut tx, self.id, ChangeItem::Message(id), ChangeOp::Update).await?;
        commit(tx).await?;
        Ok(MessageRef { id, uid: new_uid })
    }

    /// Soft-delete a live message (sets `deleted_at`); the blob is untouched.
    pub async fn expunge(&self, id: MessageId) -> Result<()> {
        let mut tx = self.begin().await?;
        let n = sqlx::query(
            "UPDATE messages SET deleted_at = ? \
             WHERE mailbox_id = ? AND id = ? AND deleted_at IS NULL",
        )
        .bind(now())
        .bind(self.id.0)
        .bind(id.0)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?
        .rows_affected();
        if n == 0 {
            return Err(not_found_message());
        }
        log_change(&mut tx, self.id, ChangeItem::Message(id), ChangeOp::Destroy).await?;
        commit(tx).await
    }

    // ----- reads -----

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
        let mut conn = self.store.pool().acquire().await.map_err(map_db_error)?;
        folder_row(&mut conn, self.id, folder).await?;
        type Row = (i64, i64, i64, i64, String, String, i64, i64);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, uid, size, internal_date, subject, from_addr, flags, thread_id \
             FROM messages WHERE mailbox_id = ? AND folder_id = ? AND uid > ? \
             AND deleted_at IS NULL ORDER BY uid LIMIT ?",
        )
        .bind(self.id.0)
        .bind(folder.0)
        .bind(i64::from(after_uid))
        .bind(limit)
        .fetch_all(&mut *conn)
        .await
        .map_err(map_db_error)?;
        rows.into_iter()
            .map(
                |(id, uid, size, internal_date, subject, from_addr, flags, thread_id)| {
                    Ok(MessageSummary {
                        id: MessageId(id),
                        uid: to_uid(uid)?,
                        size,
                        internal_date,
                        subject,
                        from_addr,
                        flags: to_flags(flags)?,
                        thread_id: ThreadId(thread_id),
                    })
                },
            )
            .collect()
    }

    /// One page of change-log entries with `seq > seq`, ordered by seq: at most `limit`
    /// entries, clamped to [`MAX_PAGE`]; `limit == 0` is `Invalid`. `more` is true if entries
    /// follow the last one returned; continue from its `seq`.
    pub async fn changes_since(&self, seq: i64, limit: u32) -> Result<ChangePage> {
        let limit = page_limit(limit)?;
        let rows: Vec<(i64, String, i64, String)> = sqlx::query_as(
            "SELECT seq, kind, item_id, op FROM change_log \
             WHERE mailbox_id = ? AND seq > ? ORDER BY seq LIMIT ?",
        )
        .bind(self.id.0)
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
