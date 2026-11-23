//! `TotpRepo`: the second factor of an account (migration 0054, docs/design-2fa.md). The webmail writes and reads it; the protocol services only ask whether the factor is on.

use async_trait::async_trait;
use rusty_core::{AccountId, Result};

/// How many recovery codes an account gets.
pub const RECOVERY_CODES: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TotpRow {
    pub sealed: Vec<u8>,
    pub confirmed: bool,
    pub last_step: Option<i64>,
}

#[async_trait]
pub trait TotpRepo: Send + Sync {
    async fn get(&self, account: AccountId) -> Result<Option<TotpRow>>;
    /// Keep a new secret, not yet confirmed; replaces an unconfirmed one, and is refused (`Error::Conflict`) when the factor is already on.
    async fn begin(&self, account: AccountId, sealed: &[u8]) -> Result<()>;
    /// The person proved a code of `step`: the factor is on, `step` is used, and these recovery hashes replace any others (one transaction).
    async fn confirm(&self, account: AccountId, step: i64, recovery_hashes: &[String]) -> Result<()>;
    /// Accept `step` if it is later than the last one used; whether it was accepted (so a code works once, even when two requests race).
    async fn advance(&self, account: AccountId, step: i64) -> Result<bool>;
    /// The recovery codes left, as (id, hash).
    async fn recovery(&self, account: AccountId) -> Result<Vec<(i64, String)>>;
    /// Use up one recovery code; whether it was still there.
    async fn spend_recovery(&self, account: AccountId, id: i64) -> Result<bool>;
    /// Turn the factor off: the secret and the recovery codes are removed.
    async fn remove(&self, account: AccountId) -> Result<()>;
    /// Whether the factor is on (confirmed): what the protocol services ask.
    async fn enabled(&self, account: AccountId) -> Result<bool>;
}

pub(crate) mod sql {
    pub const GET: &str = "SELECT sealed, confirmed_at IS NOT NULL, last_step FROM totp WHERE account_id = $1";
    pub const BEGIN: &str = "INSERT INTO totp (account_id, sealed, created_at) VALUES ($1, $2, rs_now()) ON CONFLICT (account_id) DO UPDATE SET sealed = EXCLUDED.sealed, created_at = EXCLUDED.created_at WHERE totp.confirmed_at IS NULL";
    pub const CONFIRM: &str = "UPDATE totp SET confirmed_at = rs_now(), last_step = $2 WHERE account_id = $1 AND confirmed_at IS NULL";
    pub const CLEAR_RECOVERY: &str = "DELETE FROM totp_recovery WHERE account_id = $1";
    pub const ADD_RECOVERY: &str = "INSERT INTO totp_recovery (account_id, hash) VALUES ($1, $2)";
    pub const ADVANCE: &str = "UPDATE totp SET last_step = $2 WHERE account_id = $1 AND confirmed_at IS NOT NULL AND (last_step IS NULL OR last_step < $2)";
    pub const RECOVERY: &str = "SELECT id, hash FROM totp_recovery WHERE account_id = $1 ORDER BY id";
    pub const SPEND: &str = "DELETE FROM totp_recovery WHERE account_id = $1 AND id = $2";
    pub const REMOVE: &str = "DELETE FROM totp WHERE account_id = $1";
    pub const ENABLED: &str = "SELECT EXISTS (SELECT 1 FROM totp WHERE account_id = $1 AND confirmed_at IS NOT NULL)";
}
