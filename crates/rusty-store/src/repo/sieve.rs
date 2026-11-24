//! `SieveRepo`: the Sieve scripts of an account (migration 0057, docs/design-sieve.md). The writers (the webmail, ManageSieve) put, delete and activate; the delivering service reads the active one.

use async_trait::async_trait;
use rusty_core::{AccountId, Result};

/// Scripts one account may keep, and the longest one (the parser's own limit is the same).
pub const MAX_SCRIPTS: i64 = 10;
pub const MAX_SCRIPT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptInfo {
    pub name: String,
    pub active: bool,
    pub size: usize,
}

#[async_trait]
pub trait SieveRepo: Send + Sync {
    /// The text of the script that is active, if any: what the delivering service runs.
    async fn active_body(&self, account: AccountId) -> Result<Option<String>>;
    async fn list(&self, account: AccountId) -> Result<Vec<ScriptInfo>>;
    async fn get(&self, account: AccountId, name: &str) -> Result<Option<String>>;
    /// Keep a script under a name, replacing the one that has it (and keeping its active flag); `Error::Quota` past [`MAX_SCRIPTS`] for a new name. The caller has run the parser.
    async fn put(&self, account: AccountId, name: &str, body: &str) -> Result<()>;
    /// Remove a script; whether there was one. The active script is not removed (`Error::Conflict`): deactivate it first.
    async fn delete(&self, account: AccountId, name: &str) -> Result<bool>;
    /// Make one script the active one, or none (`None`); `Error::NotFound` for a name that is not there.
    async fn set_active(&self, account: AccountId, name: Option<&str>) -> Result<()>;
}

pub(crate) mod sql {
    pub const ACTIVE: &str = "SELECT body FROM sieve_scripts WHERE account_id = $1 AND active";
    pub const LIST: &str = "SELECT name, active, length(body) FROM sieve_scripts WHERE account_id = $1 ORDER BY name";
    pub const GET: &str = "SELECT body FROM sieve_scripts WHERE account_id = $1 AND name = $2";
    pub const COUNT: &str = "SELECT count(*) FROM sieve_scripts WHERE account_id = $1";
    pub const EXISTS: &str = "SELECT EXISTS (SELECT 1 FROM sieve_scripts WHERE account_id = $1 AND name = $2)";
    pub const PUT: &str = "INSERT INTO sieve_scripts (account_id, name, body, updated_at) VALUES ($1, $2, $3, rs_now()) ON CONFLICT (account_id, name) DO UPDATE SET body = EXCLUDED.body, updated_at = EXCLUDED.updated_at";
    pub const IS_ACTIVE: &str = "SELECT active FROM sieve_scripts WHERE account_id = $1 AND name = $2";
    pub const DELETE: &str = "DELETE FROM sieve_scripts WHERE account_id = $1 AND name = $2 AND NOT active";
    pub const DEACTIVATE: &str = "UPDATE sieve_scripts SET active = false WHERE account_id = $1 AND active";
    pub const ACTIVATE: &str = "UPDATE sieve_scripts SET active = true WHERE account_id = $1 AND name = $2";
}
