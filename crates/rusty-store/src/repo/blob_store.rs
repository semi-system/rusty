//! `BlobStoreRepo`: which blob store holds the bodies of a domain (migration 0023, docs/design-s3-per-tenant.md). A domain either has a store of its own (one row whose
//! `domain_id` is that domain) or uses the platform store, row 1. The settings here are the non-secret part; credentials are sealed secrets.

use async_trait::async_trait;
use rusty_core::{DomainId, Result};

/// The identity of a blob store. Store 1 is the platform store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StoreId(pub i64);

/// The platform store: settings from the configuration file, used by every domain without a store of its own and by system mail.
pub const PLATFORM_STORE: StoreId = StoreId(1);

/// One blob store row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobStoreRow {
    pub id: StoreId,
    /// The one domain that owns the store; `None` for the platform store.
    pub domain: Option<DomainId>,
    /// `platform`, `fs` or `s3`.
    pub kind: String,
    /// Non-secret settings as JSON text.
    pub config: String,
    pub check_status: String,
    pub checked_at: Option<i64>,
}

#[async_trait]
pub trait BlobStoreRepo: Send + Sync {
    async fn get(&self, id: StoreId) -> Result<Option<BlobStoreRow>>;
    /// Every store, platform first.
    async fn list(&self) -> Result<Vec<BlobStoreRow>>;
    /// Give a domain a store of its own (`kind` `fs` or `s3`) and point it at the new store. `Conflict` when the domain already has one or already holds messages (version 1 does
    /// not move bodies between stores), `NotFound` when the domain does not exist.
    async fn create_for_domain(&self, domain: DomainId, kind: &str, config: &str) -> Result<StoreId>;
}

pub(crate) mod sql {
    pub const STORE_OF_MAILBOX: &str = "SELECT d.blob_store_id FROM mailboxes m JOIN accounts a ON a.id = m.account_id JOIN domains d ON d.id = a.domain_id WHERE m.id = $1";
    pub const GET: &str = "SELECT id, domain_id, kind, CAST(config AS TEXT), check_status, checked_at FROM blob_stores WHERE id = $1";
    pub const LIST: &str = "SELECT id, domain_id, kind, CAST(config AS TEXT), check_status, checked_at FROM blob_stores ORDER BY id";
    pub const DOMAIN_LOCK: &str = "SELECT blob_store_id FROM domains WHERE id = $1 FOR UPDATE";
    pub const DOMAIN_HAS_MESSAGES: &str =
        "SELECT EXISTS (SELECT 1 FROM messages m JOIN mailboxes mb ON mb.id = m.mailbox_id JOIN accounts a ON a.id = mb.account_id WHERE a.domain_id = $1)";
    pub const INSERT: &str = "INSERT INTO blob_stores (domain_id, kind, config, created_at) VALUES ($1, $2, CAST($3 AS JSONB), {NOW}) RETURNING id";
    pub const POINT_DOMAIN: &str = "UPDATE domains SET blob_store_id = $2 WHERE id = $1";
}
