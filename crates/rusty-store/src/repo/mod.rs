//! PG2 spike: backend-neutral repository traits at OPERATION granularity.
//!
//! One slice only (see docs/superpowers/plans/2026-11-03-postgres-backend-pg2-report.md):
//! `create_domain`, `create_account` (mailbox row + system folders), `resolve`, `create_folder`,
//! `append`/`deliver` (idempotent through a `DeliveryId`), `append_many`, `list_folder`,
//! `changes_since`.
//!
//! Division of labour (this is the point of the spike):
//! - The facades below (`SpikeDirectory`, `SpikeMailbox`) own everything that does not talk to
//!   a database: validation, limits, MIME parsing, blob IO ordering ("blob before metadata
//!   transaction"), paging limits and the result-shaping rules. They hold `Arc<dyn Repo>`.
//! - A repository method is ONE unit of work that owns its transaction. The caller passes owned
//!   or borrowed plain data and gets plain data back. No transaction handle crosses the trait.
//!   That is what makes the Postgres retry loop (40P01/40001) possible: a whole operation can
//!   be re-run because nothing outside the repository observes a half-finished attempt.
//! - Statement text that is identical up to placeholder syntax is shared (`sql` module); the
//!   Rust glue (bind and decode) is per backend.
//!
//! Nothing here is wired into the existing `Mailbox` / `Directory`; those keep working
//! untouched.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use rusty_core::{
    AccountId, BlobHash, DeliveryId, DomainId, Error, Flags, FolderId, MailboxId,
    PlanId, Result,
};
use rusty_mime::Envelope;

use crate::mailbox::{
    page_limit, validate_folder_name, ChangePage, DeliverOutcome, MessageRef, MessageSummary,
    MAX_FOLDER_DEPTH, MAX_THREAD_CANDIDATES,
};
use crate::BlobStore;

#[cfg(feature = "postgres")]
pub mod pg;
pub mod sqlite;



/// Number of system folders created with every mailbox (folder ids 1..=SYSTEM_FOLDERS).
pub const SYSTEM_FOLDERS: i64 = 5;

/// `(id, name, role)` of the system folders. Same on every backend.
pub const SYSTEM_FOLDER_TABLE: [(i64, &str, &str); 5] = [
    (1, "Inbox", "inbox"),
    (2, "Sent", "sent"),
    (3, "Drafts", "drafts"),
    (4, "Trash", "trash"),
    (5, "Spam", "spam"),
];

// ---------------------------------------------------------------- neutral data

/// Input of `create_account`. `local` is already normalized by the caller (normalization lives
/// in `rusty-directory` and stays backend-neutral).
#[derive(Debug, Clone)]
pub struct NewAccount {
    pub local: String,
    pub domain: DomainId,
    pub plan: Option<PlanId>,
    pub password_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListMember {
    Account(AccountId),
    External(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Account(AccountId),
    List(Vec<ListMember>),
}

/// A message to store: metadata parsed by the facade, content already in the blob store.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub folder: FolderId,
    pub hash: BlobHash,
    pub size: i64,
    pub flags: Flags,
    pub env: Envelope,
}

/// Idempotency key of a delivery. `created_at: None` = database clock.
#[derive(Debug, Clone, Copy)]
pub struct Delivery {
    pub id: DeliveryId,
    pub created_at: Option<i64>,
}

#[async_trait]
pub trait DirectoryRepo: Send + Sync {
    async fn create_domain(&self, name: &str) -> Result<DomainId>;
    async fn create_account(&self, new: &NewAccount) -> Result<AccountId>;
    /// `None`: no account, alias or list with this address.
    async fn resolve(&self, local: &str, domain: &str, max_members: usize)
        -> Result<Option<Resolution>>;
}

#[async_trait]
pub trait MailboxRepo: Send + Sync {
    async fn mailbox_for_account(&self, account: AccountId) -> Result<MailboxId>;
    /// `NotFound` if the folder is not in this mailbox.
    async fn folder_exists(&self, mb: MailboxId, folder: FolderId) -> Result<()>;
    async fn create_folder(
        &self,
        mb: MailboxId,
        parent: Option<FolderId>,
        name: &str,
    ) -> Result<FolderId>;
    /// Store one message (plus a `deliveries` row if `delivery` is given), one transaction.
    async fn append(
        &self,
        mb: MailboxId,
        msg: &NewMessage,
        delivery: Option<&Delivery>,
    ) -> Result<DeliverOutcome>;
    /// Store `msgs` (1..) in input order in ONE transaction: ids, uids and change seqs are
    /// consecutive in input order.
    async fn append_many(&self, mb: MailboxId, msgs: &[NewMessage]) -> Result<Vec<MessageRef>>;
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
/// `render_*` produce the dialect.
/// Only static text is ever rendered: caller data is always bound.
pub mod sql_text {
    pub const FOLDER_EXISTS: &str = "SELECT id FROM folders WHERE mailbox_id = $1 AND id = $2";
    pub const MAILBOX_FOR_ACCOUNT: &str = "SELECT id FROM mailboxes WHERE account_id = $1";
    pub const INSERT_DOMAIN: &str =
        "INSERT INTO domains (name, created_at) VALUES ($1, {NOW}) RETURNING id";
    pub const INSERT_ACCOUNT: &str = "INSERT INTO accounts \
        (local_part, domain_id, plan_id, password_hash, status, created_at) \
        VALUES ($1, $2, $3, $4, 'active', {NOW}) RETURNING id";
    /// Resolve step 1: kind 1 account, 2 alias, 3 list. `CAST(.. AS BIGINT)` because a bare
    /// literal is int4 on Postgres and the row decoder reads i64.
    pub const RESOLVE_KIND: &str = "SELECT kind, id FROM ( \
          SELECT CAST(1 AS BIGINT) AS kind, a.id AS id FROM domains d JOIN accounts a \
            ON a.domain_id = d.id WHERE d.name = $1 AND a.local_part = $2 \
          UNION ALL \
          SELECT 2, al.account_id FROM domains d JOIN aliases al \
            ON al.domain_id = d.id WHERE d.name = $1 AND al.local_part = $2 \
          UNION ALL \
          SELECT 3, l.id FROM domains d JOIN lists l \
            ON l.domain_id = d.id WHERE d.name = $1 AND l.local_part = $2 \
        ) AS u ORDER BY kind LIMIT 1";
    pub const LIST_MEMBERS: &str =
        "SELECT account_id, external FROM list_members WHERE list_id = $1 ORDER BY seq LIMIT $2";
    pub const ANCESTORS: &str = "WITH RECURSIVE up(id, parent_id, depth) AS ( \
          SELECT id, parent_id, 1 FROM folders WHERE mailbox_id = $1 AND id = $2 \
          UNION ALL \
          SELECT f.id, f.parent_id, u.depth + 1 FROM folders f JOIN up u \
            ON f.mailbox_id = $1 AND f.id = u.parent_id \
          WHERE u.depth <= $3 \
        ) SELECT id, parent_id FROM up ORDER BY depth";
    pub const INSERT_FOLDER: &str = "INSERT INTO folders \
        (mailbox_id, id, parent_id, name, role, uid_validity) VALUES ($1, $2, $3, $4, 'custom', $5)";
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
    pub const TAKE_FOLDER_ID: &str = "UPDATE mailboxes SET next_folder_id = next_folder_id + 1 \
        WHERE id = $1 RETURNING next_folder_id - 1";
    pub const TAKE_SEQ: &str = "UPDATE mailboxes SET next_change_seq = next_change_seq + 1 \
        WHERE id = $1 RETURNING next_change_seq - 1";
    pub const DELIVERY_LOOKUP: &str =
        "SELECT message_id, blob_hash FROM deliveries WHERE mailbox_id = $1 AND delivery_id = $2";
    pub const MESSAGE_UID: &str = "SELECT uid FROM messages WHERE mailbox_id = $1 AND id = $2";
    pub const LIST_FOLDER: &str = "SELECT id, uid, size, internal_date, subject, from_addr, \
        flags, thread_id FROM messages WHERE mailbox_id = $1 AND folder_id = $2 AND uid > $3 \
        AND deleted_at IS NULL ORDER BY uid LIMIT $4";
    pub const CHANGES: &str = "SELECT seq, kind, item_id, op FROM change_log \
        WHERE mailbox_id = $1 AND seq > $2 ORDER BY seq LIMIT $3";
}

// ---------------------------------------------------------------- pure helpers

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

#[cfg(feature = "postgres")]
/// Blob hash as the 32 raw bytes Postgres stores. `BlobHash` is validated lowercase hex.
pub(crate) fn hash_bytes(h: &BlobHash) -> Result<Vec<u8>> {
    hex::decode(h.as_str()).map_err(|_| Error::Invalid("invalid blob hash".into()))
}

pub(crate) fn not_found_folder() -> Error {
    Error::NotFound("folder".into())
}

pub(crate) fn not_found_mailbox() -> Error {
    Error::NotFound("mailbox".into())
}


// ---------------------------------------------------------------- facades

fn stand_in_domain_ok(name: &str) -> bool {
    // Stand-in for `rusty_directory::normalize_domain` (IDNA lives in rusty-directory, which
    // depends on this crate). Real wiring passes the normalized name.
    !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-'))
}

/// Directory facade of the spike. Validation here, SQL in the repository.
#[derive(Clone)]
pub struct SpikeDirectory {
    repo: Arc<dyn DirectoryRepo>,
}

impl SpikeDirectory {
    pub fn new(repo: Arc<dyn DirectoryRepo>) -> Self {
        Self { repo }
    }

    pub async fn create_domain(&self, name: &str) -> Result<DomainId> {
        if !stand_in_domain_ok(name) {
            return Err(Error::Invalid("invalid domain".into()));
        }
        self.repo.create_domain(name).await
    }

    pub async fn create_account(
        &self,
        local: &str,
        domain: DomainId,
        password_hash: Option<String>,
        plan: Option<PlanId>,
    ) -> Result<AccountId> {
        if local.is_empty() || local.len() > 64 {
            return Err(Error::Invalid("invalid local part".into()));
        }
        self.repo
            .create_account(&NewAccount {
                local: local.to_owned(),
                domain,
                plan,
                password_hash,
            })
            .await
    }

    pub async fn resolve(&self, local: &str, domain: &str) -> Result<Resolution> {
        self.repo
            .resolve(local, domain, 10_000)
            .await?
            .ok_or_else(|| Error::NotFound("address".into()))
    }
}

/// Mailbox facade of the spike.
#[derive(Clone)]
pub struct SpikeMailbox {
    repo: Arc<dyn MailboxRepo>,
    blobs: Arc<dyn BlobStore>,
    id: MailboxId,
    max_message_bytes: usize,
}

impl SpikeMailbox {
    pub async fn open(
        repo: Arc<dyn MailboxRepo>,
        blobs: Arc<dyn BlobStore>,
        account: AccountId,
    ) -> Result<Self> {
        let id = repo.mailbox_for_account(account).await?;
        Ok(Self {
            repo,
            blobs,
            id,
            max_message_bytes: crate::DEFAULT_MAX_MESSAGE_BYTES,
        })
    }

    pub fn id(&self) -> MailboxId {
        self.id
    }

    pub async fn create_folder(&self, parent: Option<FolderId>, name: &str) -> Result<FolderId> {
        validate_folder_name(name)?;
        self.repo.create_folder(self.id, parent, name).await
    }

    pub async fn append(&self, folder: FolderId, raw: Bytes, flags: Flags) -> Result<MessageRef> {
        match self.store(folder, raw, flags, None).await? {
            DeliverOutcome::Created(r) | DeliverOutcome::Duplicate(r) => Ok(r),
        }
    }

    pub async fn deliver(
        &self,
        folder: FolderId,
        raw: Bytes,
        flags: Flags,
        id: DeliveryId,
    ) -> Result<DeliverOutcome> {
        self.store(
            folder,
            raw,
            flags,
            Some(Delivery {
                id,
                created_at: None,
            }),
        )
        .await
    }

    /// Append every message of `raws` to `folder` in one transaction.
    pub async fn append_many(
        &self,
        folder: FolderId,
        raws: Vec<Bytes>,
        flags: Flags,
    ) -> Result<Vec<MessageRef>> {
        if raws.is_empty() || raws.len() > crate::MAX_BATCH {
            return Err(Error::Invalid("a batch holds between 1 and 1000 items".into()));
        }
        self.repo.folder_exists(self.id, folder).await?;
        let mut msgs = Vec::with_capacity(raws.len());
        for raw in raws {
            msgs.push(self.prepare(folder, raw, flags).await?);
        }
        self.repo.append_many(self.id, &msgs).await
    }

    async fn prepare(&self, folder: FolderId, raw: Bytes, flags: Flags) -> Result<NewMessage> {
        if raw.is_empty() {
            return Err(Error::Invalid("empty message".into()));
        }
        if raw.len() > self.max_message_bytes {
            return Err(Error::Quota("message exceeds the size limit".into()));
        }
        let size =
            i64::try_from(raw.len()).map_err(|_| Error::Quota("message too large".into()))?;
        let env = rusty_mime::parse_envelope(&raw)?;
        // Blob first, metadata transaction second: a failed/retried transaction leaves at worst
        // an orphan blob for the cleanup job, and a retry never repeats a blob write.
        let hash = self.blobs.put(raw).await?;
        Ok(NewMessage {
            folder,
            hash,
            size,
            flags,
            env,
        })
    }

    async fn store(
        &self,
        folder: FolderId,
        raw: Bytes,
        flags: Flags,
        delivery: Option<Delivery>,
    ) -> Result<DeliverOutcome> {
        self.repo.folder_exists(self.id, folder).await?;
        let msg = self.prepare(folder, raw, flags).await?;
        self.repo.append(self.id, &msg, delivery.as_ref()).await
    }

    pub async fn list_folder(
        &self,
        folder: FolderId,
        after_uid: u32,
        limit: u32,
    ) -> Result<Vec<MessageSummary>> {
        let limit = page_limit(limit)?;
        self.repo.list_folder(self.id, folder, after_uid, limit).await
    }

    pub async fn changes_since(&self, seq: i64, limit: u32) -> Result<ChangePage> {
        let limit = page_limit(limit)?;
        self.repo.changes_since(self.id, seq, limit).await
    }
}

// ---------------------------------------------------------------- dialect rendering

/// SQLite text: the database clock is `unixepoch()`. Placeholders are `$N` on every backend:
/// sqlx-sqlite maps `$N` to the N-th bound value (see the tripwire test
/// `sqlite_dollar_placeholders_follow_their_number`), so no placeholder rewriting is needed.
pub(crate) fn render_sqlite(sql: &str) -> String {
    sql.replace("{NOW}", "unixepoch()")
}

/// Postgres text: the database clock is `rs_now()` (`statement_timestamp()`).
#[cfg(any(test, feature = "postgres"))]
pub(crate) fn render_pg(sql: &str) -> String {
    sql.replace("{NOW}", "rs_now()")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_replaces_only_the_clock_token() {
        assert_eq!(
            render_pg("SELECT 1 WHERE a = $1 AND t = {NOW}"),
            "SELECT 1 WHERE a = $1 AND t = rs_now()"
        );
        assert_eq!(
            render_sqlite("SELECT {NOW}, $1"),
            "SELECT unixepoch(), $1"
        );
        for s in [
            sql_text::FOLDER_EXISTS,
            sql_text::RESOLVE_KIND,
            sql_text::ANCESTORS,
            sql_text::TAKE_MSG_IDS_AND_SEQS,
            sql_text::LIST_FOLDER,
            sql_text::CHANGES,
            sql_text::INSERT_ACCOUNT,
        ] {
            assert!(!s.contains('?'), "{s}");
            assert!(!render_pg(s).contains("{NOW}"), "{s}");
            assert!(!render_sqlite(s).contains("{NOW}"), "{s}");
        }
    }
}
