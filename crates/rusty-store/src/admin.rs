//! `AdminStore`: the facade over [`AdminRepo`] (admin web design, section 5). Who is an administrator, of what. It holds no credential:
//! signing in is the account's own password, checked by the directory; this only says what a signed-in account may administer.

use std::sync::Arc;

use rusty_core::{AccountId, DomainId, Error, Result};

use crate::repo::{AdminEntry, AdminGrant, AdminRepo, Revoked};

/// Most grants one `list` call returns.
pub const MAX_ADMIN_PAGE: i64 = 200;

#[derive(Clone)]
pub struct AdminStore {
    repo: Arc<dyn AdminRepo>,
}

impl AdminStore {
    pub fn with_repo(repo: Arc<dyn AdminRepo>) -> AdminStore {
        AdminStore { repo }
    }

    /// Make `account` the platform administrator (`domain` `None`) or the administrator of one domain, replacing any earlier grant.
    pub async fn grant(&self, account: AccountId, domain: Option<DomainId>) -> Result<AdminGrant> {
        self.repo.put(account, domain, None).await
    }

    #[doc(hidden)]
    pub async fn grant_at(&self, account: AccountId, domain: Option<DomainId>, now: i64) -> Result<AdminGrant> {
        if now < 0 {
            return Err(Error::Invalid("time must not be negative".into()));
        }
        self.repo.put(account, domain, Some(now)).await
    }

    /// Make the grant a helpdesk grant (`true`) or a full one (`false`); whether the account has a grant.
    pub async fn set_helpdesk(&self, account: AccountId, on: bool) -> Result<bool> {
        self.repo.set_helpdesk(account, on).await
    }

    pub async fn get(&self, account: AccountId) -> Result<Option<AdminGrant>> {
        self.repo.get(account).await
    }

    /// Remove the grant; whether there was one.
    pub async fn revoke(&self, account: AccountId) -> Result<bool> {
        self.repo.delete(account).await
    }

    /// One page of grants after `after` (`limit` is clamped to `1..=`[`MAX_ADMIN_PAGE`]).
    pub async fn list(&self, after: Option<AccountId>, limit: i64) -> Result<Vec<AdminGrant>> {
        self.repo.list(after, limit.clamp(1, MAX_ADMIN_PAGE)).await
    }

    /// One page of administrators with their addresses; `domain` `Some` lists only those of that domain.
    pub async fn list_named(&self, after: Option<AccountId>, limit: i64, domain: Option<DomainId>, text: Option<&str>) -> Result<Vec<AdminEntry>> {
        self.repo.list_named(after, limit.clamp(1, MAX_ADMIN_PAGE), domain, text).await
    }

    /// How many administrators match (the whole list, not the page).
    pub async fn count_named(&self, domain: Option<DomainId>, text: Option<&str>) -> Result<i64> {
        self.repo.count_named(domain, text).await
    }

    /// Remove the grant, but never the last platform administrator.
    pub async fn revoke_keeping_one(&self, account: AccountId) -> Result<Revoked> {
        self.repo.revoke_keeping_one(account).await
    }

    /// Whether any platform administrator exists. The first-administrator setup page is served only while this is `false`; the answer is a
    /// database fact, read each time, never a flag kept in memory.
    pub async fn has_platform_admin(&self) -> Result<bool> {
        Ok(self.repo.count_platform().await? > 0)
    }
}
