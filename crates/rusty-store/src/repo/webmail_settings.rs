//! `WebmailSettingsRepo`: the person's own settings of the webmail (migration 0053): the signature.

use async_trait::async_trait;
use rusty_core::{AccountId, Result};

/// The longest signature, in characters (the column allows the same).
pub const MAX_SIGNATURE: usize = 2000;

#[async_trait]
pub trait WebmailSettingsRepo: Send + Sync {
    /// The signature of the account; empty when none was set.
    async fn signature(&self, account: AccountId) -> Result<String>;
    async fn set_signature(&self, account: AccountId, signature: &str) -> Result<()>;
}

pub(crate) mod sql {
    pub const GET: &str = "SELECT signature FROM webmail_settings WHERE account_id = $1";
    pub const SET: &str = "INSERT INTO webmail_settings (account_id, signature, updated_at) VALUES ($1, $2, rs_now()) ON CONFLICT (account_id) DO UPDATE SET signature = EXCLUDED.signature, updated_at = EXCLUDED.updated_at";
}
