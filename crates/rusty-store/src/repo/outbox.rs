//! `OutboxRepo`: what a person sends from the webmail (migration 0051, docs/design-webmail.md). The webmail service writes a row (the composed message is already in the blob store); the submission service
//! claims it, sends the message like an authenticated submission and finishes it; the same rows are the outbox the person reads. The collector removes the finished ones after a week.

use async_trait::async_trait;
use rusty_core::{AccountId, Result};

/// A message of the outbox as stored.
#[derive(Debug, Clone)]
pub struct OutboxItem {
    pub id: i64,
    pub account: AccountId,
    /// The address it was sent from.
    pub sender: String,
    pub blob_hash: String,
    pub size: i64,
    /// The envelope recipients.
    pub recipients: Vec<String>,
    pub subject: String,
    /// `queued`, `sending`, `sent` or `failed`.
    pub state: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

/// How many messages of one account may wait in the outbox at once.
pub const MAX_QUEUED_PER_ACCOUNT: i64 = 50;

#[async_trait]
pub trait OutboxRepo: Send + Sync {
    /// Queue a message of `account`; refused (`Error::Quota`) when the account already has [`MAX_QUEUED_PER_ACCOUNT`] waiting. Returns the id.
    async fn enqueue(&self, account: AccountId, sender: &str, blob_hash: &str, size: i64, recipients: &[String], subject: &str) -> Result<i64>;
    /// The newest messages of one account's outbox, at most `limit`.
    async fn list(&self, account: AccountId, limit: i64) -> Result<Vec<OutboxItem>>;
    /// Remove a message of this account that has not been taken yet (`queued`) or has failed; whether one was removed.
    async fn discard(&self, account: AccountId, id: i64) -> Result<bool>;
    /// The sender takes the oldest queued message: it becomes `sending`.
    async fn claim(&self) -> Result<Option<OutboxItem>>;
    /// The sender is done: `sent` or `failed` (with the fixed word of why).
    async fn finish(&self, id: i64, state: &str, error: Option<&str>) -> Result<()>;
    /// A message left `sending` by a sender that stopped becomes `failed` after this many seconds.
    async fn fail_stuck(&self, older_than_secs: i64) -> Result<u64>;
    /// Remove the finished messages older than this many seconds; how many, at most `limit`.
    async fn purge_finished(&self, older_than_secs: i64, limit: i64) -> Result<u64>;
}

pub(crate) mod sql {
    pub const COUNT_QUEUED: &str = "SELECT count(*) FROM webmail_outbox WHERE account_id = $1 AND state IN ('queued', 'sending')";
    pub const ENQUEUE: &str = "INSERT INTO webmail_outbox (account_id, sender, blob_hash, size, recipients, subject, created_at) VALUES ($1, $2, $3, $4, $5, $6, rs_now()) RETURNING id";
    pub const LIST: &str = "SELECT id, account_id, sender, blob_hash, size, recipients, subject, state, error, created_at, finished_at FROM webmail_outbox WHERE account_id = $1 ORDER BY id DESC LIMIT $2";
    pub const DISCARD: &str = "DELETE FROM webmail_outbox WHERE account_id = $1 AND id = $2 AND state IN ('queued', 'failed')";
    pub const CLAIM: &str = "UPDATE webmail_outbox SET state = 'sending', finished_at = NULL WHERE id = (SELECT id FROM webmail_outbox WHERE state = 'queued' ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id, account_id, sender, blob_hash, size, recipients, subject, state, error, created_at, finished_at";
    pub const FINISH: &str = "UPDATE webmail_outbox SET state = $2, error = $3, finished_at = rs_now() WHERE id = $1 AND state = 'sending'";
    pub const FAIL_STUCK: &str = "UPDATE webmail_outbox SET state = 'failed', error = 'failed', finished_at = rs_now() WHERE state = 'sending' AND created_at < rs_now() - $1";
    pub const PURGE: &str = "DELETE FROM webmail_outbox WHERE id IN (SELECT id FROM webmail_outbox WHERE state IN ('sent', 'failed') AND finished_at < rs_now() - $1 ORDER BY id LIMIT $2)";
}
