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
}

#[async_trait]
pub trait OutboundEnqueue: Send + Sync {
    /// Make the message durable in the queue. Returning `Ok` is the promise a `250` rests on.
    async fn enqueue(&self, msg: OutboundMessage) -> Result<()>;
}
