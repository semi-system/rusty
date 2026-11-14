//! `SealedRepo`: storage of sealed secrets (migration 0019). The repository moves bytes: it never opens a blob, never sees a
//! plaintext, and does not know the sealed format (`rusty-seal` owns that). Validation of what may be stored is the facades' job
//! (`crate::sealed`); the table's constraints are the second line.

use async_trait::async_trait;
use rusty_core::Result;

/// A stored secret, ciphertext included: what the outbound service reads.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedRow {
    pub owner_kind: String,
    pub owner_id: i64,
    pub purpose: String,
    /// The id of the sealing key (16 hex digits) the blob was sealed to.
    pub key_id: String,
    pub sealed: Vec<u8>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl std::fmt::Debug for SealedRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SealedRow({}/{}/{}, key {}, {} bytes)", self.owner_kind, self.owner_id, self.purpose, self.key_id, self.sealed.len())
    }
}

/// What may be known about a stored secret without reading it: the admin sees this and nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedMeta {
    pub owner_kind: String,
    pub owner_id: i64,
    pub purpose: String,
    pub key_id: String,
    pub size: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[async_trait]
pub trait SealedRepo: Send + Sync {
    /// Insert or replace the whole row; `created_at` of an existing row stays. `now` overrides the database clock (tests).
    async fn put(&self, kind: &str, id: i64, purpose: &str, key_id: &str, sealed: &[u8], now: Option<i64>) -> Result<SealedMeta>;
    /// The secret, ciphertext included.
    async fn get(&self, kind: &str, id: i64, purpose: &str) -> Result<Option<SealedRow>>;
    /// The secrets of one owner without their ciphertext, ordered by purpose.
    async fn meta(&self, kind: &str, id: i64) -> Result<Vec<SealedMeta>>;
    /// Remove one secret; whether there was one.
    async fn delete(&self, kind: &str, id: i64, purpose: &str) -> Result<bool>;
}

pub(crate) mod sql {
    pub const PUT: &str = "INSERT INTO sealed_secrets (owner_kind, owner_id, purpose, key_id, sealed, created_at, updated_at) \
        VALUES ($1, $2, $3, $4, $5, COALESCE($6, {NOW}), COALESCE($6, {NOW})) \
        ON CONFLICT (owner_kind, owner_id, purpose) DO UPDATE SET key_id = excluded.key_id, sealed = excluded.sealed, \
        updated_at = excluded.updated_at \
        RETURNING owner_kind, owner_id, purpose, key_id, CAST(octet_length(sealed) AS BIGINT), created_at, updated_at";
    pub const GET: &str = "SELECT owner_kind, owner_id, purpose, key_id, sealed, created_at, updated_at FROM sealed_secrets \
        WHERE owner_kind = $1 AND owner_id = $2 AND purpose = $3";
    pub const META: &str = "SELECT owner_kind, owner_id, purpose, key_id, CAST(octet_length(sealed) AS BIGINT), created_at, updated_at \
        FROM sealed_secrets WHERE owner_kind = $1 AND owner_id = $2 ORDER BY purpose";
    pub const DELETE: &str = "DELETE FROM sealed_secrets WHERE owner_kind = $1 AND owner_id = $2 AND purpose = $3";
}
