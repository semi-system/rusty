//! `QuotaPolicy`: consulted by delivery and append in the service layer before a message is
//! stored. The default enforces only the per-message size cap; per-account and per-tenant quotas
//! are other implementations (the store's `MailboxLimits` stays the hard backstop).

use rusty_core::{AccountId, Error, Result};

pub trait QuotaPolicy: Send + Sync {
    /// May `account` store a message of `size` bytes? `Error::Quota` when not.
    fn check_append(&self, account: AccountId, size: u64) -> Result<()>;
}

/// Size cap only.
#[derive(Debug, Clone, Copy)]
pub struct SizeCap {
    pub max_message_bytes: u64,
}

impl QuotaPolicy for SizeCap {
    fn check_append(&self, _account: AccountId, size: u64) -> Result<()> {
        if size > self.max_message_bytes {
            return Err(Error::Quota("message is larger than the size limit".into()));
        }
        Ok(())
    }
}
