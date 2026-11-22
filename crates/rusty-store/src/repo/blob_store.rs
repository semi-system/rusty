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
    /// The room the operator says the store has (migration 0042); `None`: not declared.
    pub capacity_bytes: Option<i64>,
    /// What the last check found: bytes held, bytes promised as quotas, accounts without a quota, and the verdict (`unset`, `ok`, `overcommitted`, `nearly_full`, `full`).
    pub used_bytes: Option<i64>,
    pub promised_bytes: Option<i64>,
    pub unlimited_accounts: Option<i64>,
    pub capacity_status: String,
}

/// What a tenant holds and has been promised: the input of the capacity verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commitments {
    pub used_bytes: i64,
    pub promised_bytes: i64,
    pub unlimited_accounts: i64,
}

#[async_trait]
pub trait BlobStoreRepo: Send + Sync {
    async fn get(&self, id: StoreId) -> Result<Option<BlobStoreRow>>;
    /// The store a domain owns, if it has one.
    async fn for_domain(&self, domain: DomainId) -> Result<Option<BlobStoreRow>>;
    /// Replace the settings of a store. `Conflict` while the store holds any blob row (the bodies would be left where the old settings point); `NotFound` for an unknown or
    /// platform store. Credentials are not here: they are rotated through the sealed secrets at any time.
    async fn update_config(&self, id: StoreId, config: &str) -> Result<()>;
    /// Record the result of a check (`status` is a fixed category, `ok` or one of `S3Fail::name`), at `now` or the database clock.
    async fn record_check(&self, id: StoreId, status: &str, now: Option<i64>) -> Result<()>;
    /// Declare the room of a tenant's store (`None` clears it). `NotFound` for an unknown or the platform store. The verdict is not changed until the next check.
    async fn set_capacity(&self, id: StoreId, capacity: Option<i64>) -> Result<()>;
    /// The bytes the mailboxes of a domain hold and the sum of the quotas its accounts have (account, plan, domain, then the global layer; an account with none counts as unlimited).
    async fn commitments(&self, domain: DomainId) -> Result<Commitments>;
    /// Record what a check found about room.
    async fn record_capacity(&self, id: StoreId, found: Commitments, status: &str) -> Result<()>;
    /// Every store, platform first.
    async fn list(&self) -> Result<Vec<BlobStoreRow>>;
    /// Give a domain a store of its own (`kind` `fs` or `s3`) and point it at the new store. `Conflict` when the domain already has one or already holds messages (version 1 does
    /// not move bodies between stores), `NotFound` when the domain does not exist.
    async fn create_for_domain(&self, domain: DomainId, kind: &str, config: &str) -> Result<StoreId>;
}

pub(crate) mod sql {
    pub const STORE_OF_MAILBOX: &str = "SELECT d.blob_store_id FROM mailboxes m JOIN accounts a ON a.id = m.account_id JOIN domains d ON d.id = a.domain_id WHERE m.id = $1";
    pub const GET: &str = "SELECT id, domain_id, kind, CAST(config AS TEXT), check_status, checked_at, capacity_bytes, used_bytes, promised_bytes, unlimited_accounts, capacity_status FROM blob_stores WHERE id = $1";
    pub const FOR_DOMAIN: &str = "SELECT id, domain_id, kind, CAST(config AS TEXT), check_status, checked_at, capacity_bytes, used_bytes, promised_bytes, unlimited_accounts, capacity_status FROM blob_stores WHERE domain_id = $1";
    pub const HAS_BLOBS: &str = "SELECT EXISTS (SELECT 1 FROM blobs WHERE store_id = $1)";
    pub const UPDATE_CONFIG: &str = "UPDATE blob_stores SET config = CAST($2 AS JSONB), check_status = 'unchecked', checked_at = NULL WHERE id = $1 AND domain_id IS NOT NULL";
    pub const RECORD_CHECK: &str = "UPDATE blob_stores SET check_status = $2, checked_at = CASE WHEN $2 = 'unchecked' THEN NULL ELSE COALESCE($3, {NOW}) END WHERE id = $1";
    pub const LIST: &str = "SELECT id, domain_id, kind, CAST(config AS TEXT), check_status, checked_at, capacity_bytes, used_bytes, promised_bytes, unlimited_accounts, capacity_status FROM blob_stores ORDER BY id";
    pub const SET_CAPACITY: &str = "UPDATE blob_stores SET capacity_bytes = $2 WHERE id = $1 AND domain_id IS NOT NULL";
    pub const RECORD_CAPACITY: &str = "UPDATE blob_stores SET used_bytes = $2, promised_bytes = $3, unlimited_accounts = $4, capacity_status = $5 WHERE id = $1";
    pub const COMMITMENTS: &str = "SELECT CAST(COALESCE(sum(COALESCE(m.used_bytes, 0)), 0) AS BIGINT), CAST(COALESCE(sum(x.q), 0) AS BIGINT), CAST(count(*) FILTER (WHERE x.q IS NULL) AS BIGINT) \
        FROM (SELECT a.id, CAST(COALESCE(a.config->>'quota_bytes', p.config->>'quota_bytes', d.config->>'quota_bytes', g.config->>'quota_bytes') AS BIGINT) AS q \
              FROM accounts a JOIN domains d ON d.id = a.domain_id LEFT JOIN plans p ON p.id = a.plan_id LEFT JOIN plans g ON g.name = $2 WHERE a.domain_id = $1) x \
        LEFT JOIN mailboxes m ON m.account_id = x.id";
    pub const DOMAIN_LOCK: &str = "SELECT blob_store_id FROM domains WHERE id = $1 FOR UPDATE";
    pub const DOMAIN_HAS_MESSAGES: &str =
        "SELECT EXISTS (SELECT 1 FROM messages m JOIN mailboxes mb ON mb.id = m.mailbox_id JOIN accounts a ON a.id = mb.account_id WHERE a.domain_id = $1)";
    pub const INSERT: &str = "INSERT INTO blob_stores (domain_id, kind, config, created_at) VALUES ($1, $2, CAST($3 AS JSONB), {NOW}) RETURNING id";
    pub const POINT_DOMAIN: &str = "UPDATE domains SET blob_store_id = $2 WHERE id = $1";
}
