//! The hand-off from submission (U4) to the outbound queue (U5). Defined here so both sides
//! depend on this crate and on nothing else new; implemented by the queue in U5.

use async_trait::async_trait;
use rusty_core::{BlobHash, Result};

/// A message accepted for delivery to remote recipients. The body is already in the blob store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundMessage {
    pub blob: BlobHash,
    /// Envelope sender; empty for a null sender (no DSN is ever generated for those).
    pub envelope_from: String,
    /// Remote recipients only (local ones are delivered directly).
    pub recipients: Vec<String>,
    /// Domain of the authenticated sender: the tenant the queue accounts the job to.
    pub sender_domain: String,
    /// The authenticated account that submitted it: the queue's per-account caps and budgets apply.
    pub account: i64,
    /// Idempotency key of the SUBMISSION: derived from the sender, the account, the envelope and the
    /// ORIGINAL bytes the client sent (not from what we store, which carries our arrival time). A
    /// client that retries after a temporary failure presents the same key, so the queue creates no
    /// second job. Empty: no idempotency.
    pub submission_key: String,
    /// Size of the message in bytes (the account's budget).
    pub bytes: u64,
}

#[async_trait]
pub trait OutboundEnqueue: Send + Sync {
    /// Make the message durable in the queue. Returning `Ok` is the promise a `250` rests on.
    async fn enqueue(&self, msg: OutboundMessage) -> Result<()>;
}
