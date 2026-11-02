//! `Mailbox`: the high-level facade over one account's folders, messages and change log.
//!
//! Invariants:
//! - Every write runs in one `BEGIN IMMEDIATE` transaction; the counters
//!   `mailboxes.next_msg_id`, `mailboxes.next_change_seq` and `folders.next_uid` are read and
//!   incremented inside it, so ids, uids and change seqs are gapless and monotonic.
//! - Every lookup is scoped by `mailbox_id`: ids of another mailbox are `NotFound`.
//! - `append` writes the blob before the metadata transaction; if the transaction fails the
//!   blob is an orphan for the cleanup job (spec §5).
//! - Nothing here logs message content; error texts never echo caller input.

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use rand_core::{OsRng, RngCore};
use rusty_core::{AccountId, Error, Flags, FolderId, MailboxId, MessageId, Result};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSummary {
    pub id: MessageId,
    pub uid: u32,
    pub size: i64,
    pub internal_date: i64,
    pub subject: String,
    pub from_addr: String,
    pub flags: Flags,
    pub thread_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Message,
    Folder,
}

impl ChangeKind {
    fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Message => "message",
            ChangeKind::Folder => "folder",
        }
    }

    fn parse(s: &str) -> Result<ChangeKind> {
        match s {
            "message" => Ok(ChangeKind::Message),
            "folder" => Ok(ChangeKind::Folder),
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
    pub kind: ChangeKind,
    pub item_id: i64,
    pub op: ChangeOp,
}

/// One account's mailbox. Cheap to clone; clones share the pool and blob store.
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

/// Random UIDVALIDITY in 1..=u32::MAX.
fn random_uid_validity() -> Result<i64> {
    loop {
        let mut b = [0u8; 4];
        OsRng
            .try_fill_bytes(&mut b)
            .map_err(|_| Error::storage("random source failed"))?;
        let v = u32::from_le_bytes(b);
        if v != 0 {
            return Ok(i64::from(v));
        }
    }
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
    kind: ChangeKind,
    item_id: i64,
    op: ChangeOp,
) -> Result<i64> {
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
    .bind(kind.as_str())
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
    let mut cur = Some(start.0);
    let mut depth = 0usize;
    while let Some(id) = cur {
        if forbidden.is_some_and(|f| f.0 == id) {
            return Err(Error::Conflict(
                "cannot move a folder into itself or a descendant".into(),
            ));
        }
        depth += 1;
        if depth > MAX_FOLDER_DEPTH {
            return Err(Error::Invalid("folder tree too deep".into()));
        }
        cur = folder_row(conn, mb, FolderId(id)).await?.0;
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
async fn find_thread(
    conn: &mut SqliteConnection,
    mb: MailboxId,
    env: &Envelope,
) -> Result<Option<i64>> {
    let candidates = env
        .references
        .iter()
        .rev()
        .chain(env.in_reply_to.iter())
        .take(MAX_THREAD_CANDIDATES);
    for mid in candidates {
        let t: Option<i64> = sqlx::query_scalar(
            "SELECT thread_id FROM messages WHERE mailbox_id = ? AND message_id_hdr = ? \
             ORDER BY id LIMIT 1",
        )
        .bind(mb.0)
        .bind(mid)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_db_error)?;
        if t.is_some() {
            return Ok(t);
        }
    }
    Ok(None)
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

    async fn begin(&self) -> Result<Transaction<'static, Sqlite>> {
        self.store.begin_write().await
    }

    // ----- folders -----

    pub async fn create_folder(&self, parent: Option<FolderId>, name: &str) -> Result<FolderId> {
        validate_folder_name(name)?;
        let uid_validity = random_uid_validity()?;
        let mut tx = self.begin().await?;
        if let Some(p) = parent {
            let depth = ancestor_depth(&mut tx, self.id, p, None).await?;
            if depth + 1 > MAX_FOLDER_DEPTH {
                return Err(Error::Invalid("folder tree too deep".into()));
            }
        }
        let id: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) + 1 FROM folders WHERE mailbox_id = ?")
                .bind(self.id.0)
                .fetch_one(&mut *tx)
                .await
                .map_err(map_db_error)?;
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
        log_change(&mut tx, self.id, ChangeKind::Folder, id, ChangeOp::Create).await?;
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
        log_change(&mut tx, self.id, ChangeKind::Folder, id.0, ChangeOp::Update).await?;
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
        log_change(&mut tx, self.id, ChangeKind::Folder, id.0, ChangeOp::Update).await?;
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
        log_change(
            &mut tx,
            self.id,
            ChangeKind::Folder,
            id.0,
            ChangeOp::Destroy,
        )
        .await?;
        commit(tx).await
    }

    // ----- messages -----

    /// Store a raw message in `folder`. Empty input is `Invalid` and input over
    /// `max_message_bytes` is `Quota`, both before anything is written. The folder is
    /// checked before the blob write (and again inside the transaction).
    pub async fn append(&self, folder: FolderId, raw: Bytes, flags: Flags) -> Result<MessageRef> {
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
        log_change(&mut tx, self.id, ChangeKind::Message, id, ChangeOp::Create).await?;
        commit(tx).await?;
        Ok(MessageRef {
            id: MessageId(id),
            uid,
        })
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
        log_change(
            &mut tx,
            self.id,
            ChangeKind::Message,
            id.0,
            ChangeOp::Update,
        )
        .await?;
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
        log_change(
            &mut tx,
            self.id,
            ChangeKind::Message,
            id.0,
            ChangeOp::Update,
        )
        .await?;
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
        log_change(
            &mut tx,
            self.id,
            ChangeKind::Message,
            id.0,
            ChangeOp::Update,
        )
        .await?;
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
        log_change(
            &mut tx,
            self.id,
            ChangeKind::Message,
            id.0,
            ChangeOp::Update,
        )
        .await?;
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
        log_change(
            &mut tx,
            self.id,
            ChangeKind::Message,
            id.0,
            ChangeOp::Destroy,
        )
        .await?;
        commit(tx).await
    }

    // ----- reads -----

    /// Live messages of `folder`, ordered by uid.
    pub async fn list_folder(&self, folder: FolderId) -> Result<Vec<MessageSummary>> {
        let mut conn = self.store.pool().acquire().await.map_err(map_db_error)?;
        folder_row(&mut conn, self.id, folder).await?;
        type Row = (i64, i64, i64, i64, String, String, i64, i64);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, uid, size, internal_date, subject, from_addr, flags, thread_id \
             FROM messages WHERE mailbox_id = ? AND folder_id = ? AND deleted_at IS NULL \
             ORDER BY uid",
        )
        .bind(self.id.0)
        .bind(folder.0)
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
                        thread_id,
                    })
                },
            )
            .collect()
    }

    /// Change-log entries with `seq > seq`, ordered by seq.
    pub async fn changes_since(&self, seq: i64) -> Result<Vec<Change>> {
        let rows: Vec<(i64, String, i64, String)> = sqlx::query_as(
            "SELECT seq, kind, item_id, op FROM change_log \
             WHERE mailbox_id = ? AND seq > ? ORDER BY seq",
        )
        .bind(self.id.0)
        .bind(seq)
        .fetch_all(self.store.pool())
        .await
        .map_err(map_db_error)?;
        rows.into_iter()
            .map(|(seq, kind, item_id, op)| {
                Ok(Change {
                    seq,
                    kind: ChangeKind::parse(&kind)?,
                    item_id,
                    op: ChangeOp::parse(&op)?,
                })
            })
            .collect()
    }
}
