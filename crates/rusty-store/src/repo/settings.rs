//! `SettingsRepo`: the platform-wide protocol switches (migration 0030, docs/design-protocols.md). A protocol with no row is on.

use async_trait::async_trait;
use rusty_core::Result;

/// The optional protocols that can be switched, by key (the closed list of the table's check).
pub const PROTOCOLS: &[&str] = &["pop3"];

#[async_trait]
pub trait SettingsRepo: Send + Sync {
    /// Every switch with a stored value, by protocol name (`pop3`), not the key.
    async fn protocols(&self) -> Result<Vec<(String, bool)>>;
    /// Switch a protocol on or off. `protocol` must be one of [`PROTOCOLS`]. `now` overrides the clock (tests).
    async fn set_protocol(&self, protocol: &str, enabled: bool, now: Option<i64>) -> Result<()>;
}

pub(crate) mod sql {
    pub const LIST: &str = "SELECT key, enabled FROM platform_settings ORDER BY key";
    pub const SET: &str = "INSERT INTO platform_settings (key, enabled, updated_at) VALUES ($1, $2, COALESCE($3, {NOW})) \
        ON CONFLICT (key) DO UPDATE SET enabled = excluded.enabled, updated_at = excluded.updated_at";
}
