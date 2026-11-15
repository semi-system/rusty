//! `RelayRepo`: the per-domain relay settings (migration 0020). The password is a sealed secret, stored through the sealed facades, never here.
//! The checks of what may be stored belong to the facade ([`crate::RelayStore`], with `rusty_core::relay_policy`); the table's constraints are
//! the second line.

use async_trait::async_trait;
use rusty_core::{DomainId, Result};

/// One stored relay. Whether a password is set is known from `sealed_secrets` by the facade, not from this table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayRow {
    pub domain: DomainId,
    pub host: String,
    pub port: u16,
    /// `starttls`, `implicit` or `none`.
    pub tls: String,
    pub username: Option<String>,
    pub spf_include: Option<String>,
    /// The fixed category of the last connection check, with its time.
    pub check_status: Option<String>,
    pub checked_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[async_trait]
pub trait RelayRepo: Send + Sync {
    /// Insert or replace the relay of a domain (a replaced relay forgets the last check: it described another server). `now` overrides the clock (tests).
    async fn put(&self, row: &RelayRow, now: Option<i64>) -> Result<RelayRow>;
    async fn get(&self, domain: DomainId) -> Result<Option<RelayRow>>;
    /// Remove the relay; whether there was one.
    async fn delete(&self, domain: DomainId) -> Result<bool>;
    /// Record the outcome of a connection check (a fixed category). `false` when the domain has no relay.
    async fn record_check(&self, domain: DomainId, status: &str, now: Option<i64>) -> Result<bool>;
}

pub(crate) mod sql {
    pub const PUT: &str = "INSERT INTO domain_relays (domain_id, host, port, tls, username, spf_include, created_at, updated_at) \
        VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, {NOW}), COALESCE($7, {NOW})) \
        ON CONFLICT (domain_id) DO UPDATE SET host = excluded.host, port = excluded.port, tls = excluded.tls, username = excluded.username, \
        spf_include = excluded.spf_include, check_status = NULL, checked_at = NULL, updated_at = excluded.updated_at \
        RETURNING domain_id, host, port, tls, username, spf_include, check_status, checked_at, created_at, updated_at";
    pub const GET: &str = "SELECT domain_id, host, port, tls, username, spf_include, check_status, checked_at, created_at, updated_at \
        FROM domain_relays WHERE domain_id = $1";
    pub const DELETE: &str = "DELETE FROM domain_relays WHERE domain_id = $1";
    pub const RECORD: &str = "UPDATE domain_relays SET check_status = $2, checked_at = COALESCE($3, {NOW}) WHERE domain_id = $1";
}
