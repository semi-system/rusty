//! `RulesRepo`: the book-keeping of automatic replies (migration 0043, docs/design-mail-rules.md).

use async_trait::async_trait;
use rusty_core::Result;

#[async_trait]
pub trait RulesRepo: Send + Sync {
    /// Claim the right to answer `sender` for `account`: `true` when no reply went to this sender within `window_secs` (the time is recorded in the same statement, so two deliveries at once
    /// cannot both answer), `false` when one did.
    async fn claim_vacation_reply(&self, account: i64, sender: &str, window_secs: i64) -> Result<bool>;
    /// Remove rows older than `older_than` (unix seconds), at most `limit`; how many.
    async fn purge_vacation_replies(&self, older_than: i64, limit: i64) -> Result<u64>;
}

pub(crate) mod sql {
    pub const CLAIM: &str = "INSERT INTO vacation_replies (account_id, sender, at) VALUES ($1, $2, rs_now()) \
        ON CONFLICT (account_id, sender) DO UPDATE SET at = rs_now() WHERE vacation_replies.at <= rs_now() - $3";
    pub const PURGE: &str = "DELETE FROM vacation_replies WHERE (account_id, sender) IN (SELECT account_id, sender FROM vacation_replies WHERE at < $1 ORDER BY at LIMIT $2)";
}
