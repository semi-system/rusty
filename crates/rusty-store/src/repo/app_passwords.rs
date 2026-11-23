//! `AppPasswordRepo`: the app passwords of an account (migration 0052, docs/design-app-passwords.md). The webmail creates, lists and revokes them for the signed-in person; the protocol services read the hashes
//! to check a login and note when one was last used.

use async_trait::async_trait;
use rusty_core::{AccountId, Result};

/// How many app passwords one account may have.
pub const MAX_APP_PASSWORDS: i64 = 10;

/// What the person sees of an app password: never the password, never its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPassword {
    pub id: i64,
    pub label: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

#[async_trait]
pub trait AppPasswordRepo: Send + Sync {
    /// Add one (`Error::Quota` past [`MAX_APP_PASSWORDS`]); returns its id.
    async fn create(&self, account: AccountId, label: &str, hash: &str) -> Result<i64>;
    async fn list(&self, account: AccountId) -> Result<Vec<AppPassword>>;
    /// The hashes to check a login against, with their ids.
    async fn hashes(&self, account: AccountId) -> Result<Vec<(i64, String)>>;
    /// Note that this one was used now (never stops a login if it fails).
    async fn touch(&self, id: i64) -> Result<()>;
    /// Revoke one of this account's; whether there was one.
    async fn revoke(&self, account: AccountId, id: i64) -> Result<bool>;
}

pub(crate) mod sql {
    pub const COUNT: &str = "SELECT count(*) FROM app_passwords WHERE account_id = $1";
    pub const CREATE: &str = "INSERT INTO app_passwords (account_id, label, hash, created_at) VALUES ($1, $2, $3, rs_now()) RETURNING id";
    pub const LIST: &str = "SELECT id, label, created_at, last_used_at FROM app_passwords WHERE account_id = $1 ORDER BY id";
    pub const HASHES: &str = "SELECT id, hash FROM app_passwords WHERE account_id = $1 ORDER BY id";
    pub const TOUCH: &str = "UPDATE app_passwords SET last_used_at = rs_now() WHERE id = $1";
    pub const REVOKE: &str = "DELETE FROM app_passwords WHERE account_id = $1 AND id = $2";
}
