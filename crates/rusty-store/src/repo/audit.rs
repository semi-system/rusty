//! `AuditRepo`: the audit log of the admin web (migration 0040, docs/design-audit.md).

use async_trait::async_trait;
use rusty_core::Result;

/// One change to write.
#[derive(Debug, Clone)]
pub struct NewAuditRow {
    pub actor_id: Option<i64>,
    pub actor: String,
    pub domain_id: Option<i64>,
    pub ip: String,
    pub method: String,
    pub route: String,
    pub path: String,
    pub status: i32,
}

/// One change as read back.
#[derive(Debug, Clone)]
pub struct AuditRow {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub domain_id: Option<i64>,
    pub ip: String,
    pub method: String,
    pub route: String,
    pub path: String,
    pub status: i32,
}

#[async_trait]
pub trait AuditRepo: Send + Sync {
    /// Write one row (the time is the database's).
    async fn record(&self, row: &NewAuditRow) -> Result<()>;
    /// Newest first, optionally of one domain, with `id < before`.
    async fn list(&self, domain: Option<i64>, before: Option<i64>, limit: i64) -> Result<Vec<AuditRow>>;
    /// Remove up to `limit` rows older than `older_than` (seconds); returns how many went.
    async fn purge(&self, older_than: i64, limit: i64) -> Result<u64>;
}

pub(crate) mod sql {
    pub const RECORD: &str = "INSERT INTO audit_log (at, actor_id, actor, domain_id, ip, method, route, path, status) VALUES (rs_now(), $1, $2, $3, $4, $5, $6, $7, $8)";
    pub const LIST: &str = "SELECT id, at, actor, domain_id, ip, method, route, path, status FROM audit_log \
        WHERE (CAST($1 AS BIGINT) IS NULL OR domain_id = $1) AND id < COALESCE($2, CAST(9223372036854775807 AS BIGINT)) ORDER BY id DESC LIMIT $3";
    pub const PURGE: &str = "DELETE FROM audit_log WHERE id IN (SELECT id FROM audit_log WHERE at < $1 ORDER BY id LIMIT $2)";
}
