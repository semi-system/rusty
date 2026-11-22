//! `AdminRepo`: who may use the admin web (migration 0021). An administrator is an ordinary account with a row here: `domain` `None` is a
//! platform administrator, `Some(d)` the administrator of that one domain (an account of that domain: the table's trigger enforces it).
//! The password is the account's own; nothing in this table is a credential.

use async_trait::async_trait;
use rusty_core::{AccountId, DomainId, Result};

/// One grant, with the names the admin pages and the CLI show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminGrant {
    pub account: AccountId,
    /// `None`: platform administrator.
    pub domain: Option<DomainId>,
    pub created_at: i64,
    /// A helpdesk grant (migration 0041): only for a domain administrator, who then may do little more than reset a password.
    pub helpdesk: bool,
}

/// A grant with the address it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminEntry {
    pub grant: AdminGrant,
    /// `local@domain` of the account.
    pub address: String,
}

/// What [`AdminRepo::revoke_keeping_one`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revoked {
    Removed,
    /// The account was not an administrator.
    NotAdmin,
    /// It is the only platform administrator: nothing was removed.
    LastPlatform,
}

#[async_trait]
pub trait AdminRepo: Send + Sync {
    /// Give an account a grant, replacing the one it had. `now` overrides the clock (tests). A missing account or domain is `NotFound`;
    /// a domain administrator that is not an account of the domain is `Invalid`.
    async fn put(&self, account: AccountId, domain: Option<DomainId>, now: Option<i64>) -> Result<AdminGrant>;
    async fn get(&self, account: AccountId) -> Result<Option<AdminGrant>>;
    /// Make the grant of `account` a helpdesk grant, or a full one again. `Invalid` for a platform grant; `false`: the account has no grant.
    async fn set_helpdesk(&self, account: AccountId, on: bool) -> Result<bool>;
    /// Remove the grant; whether there was one.
    async fn delete(&self, account: AccountId) -> Result<bool>;
    /// Every grant, ordered by account id, after `after`, at most `limit`.
    async fn list(&self, after: Option<AccountId>, limit: i64) -> Result<Vec<AdminGrant>>;
    /// Grants with the address of the account, after `after`, at most `limit`; `domain` `Some` keeps only the administrators of that domain.
    async fn list_named(&self, after: Option<AccountId>, limit: i64, domain: Option<DomainId>, text: Option<&str>) -> Result<Vec<AdminEntry>>;
    /// How many grants `list_named` lists in all (the same `domain` and `text`).
    async fn count_named(&self, domain: Option<DomainId>, text: Option<&str>) -> Result<i64>;
    /// Remove the grant unless it is the last platform grant (checked and removed under one lock, so two revocations cannot both pass).
    async fn revoke_keeping_one(&self, account: AccountId) -> Result<Revoked>;
    /// How many platform administrators exist (the setup page exists only while there is none).
    async fn count_platform(&self) -> Result<i64>;
}

pub(crate) mod sql {
    pub const PUT: &str = "INSERT INTO admin_grants (account_id, domain_id, created_at) VALUES ($1, $2, COALESCE($3, {NOW})) \
        ON CONFLICT (account_id) DO UPDATE SET domain_id = excluded.domain_id, helpdesk = false \
        RETURNING account_id, domain_id, created_at, helpdesk";
    pub const GET: &str = "SELECT account_id, domain_id, created_at, helpdesk FROM admin_grants WHERE account_id = $1";
    pub const SET_HELPDESK: &str = "UPDATE admin_grants SET helpdesk = $2 WHERE account_id = $1";
    pub const DELETE: &str = "DELETE FROM admin_grants WHERE account_id = $1";
    pub const LIST: &str = "SELECT account_id, domain_id, created_at, helpdesk FROM admin_grants WHERE account_id > $1 ORDER BY account_id LIMIT $2";
    pub const LIST_NAMED: &str = "SELECT g.account_id, g.domain_id, g.created_at, g.helpdesk, a.local_part || '@' || d.name FROM admin_grants g \n        JOIN accounts a ON a.id = g.account_id JOIN domains d ON d.id = a.domain_id \n        WHERE g.account_id > $1 AND (CAST($3 AS BIGINT) IS NULL OR g.domain_id = $3) AND (CAST($4 AS TEXT) IS NULL OR strpos(a.local_part || '@' || d.name, CAST($4 AS TEXT)) > 0) ORDER BY g.account_id LIMIT $2";
    pub const COUNT_NAMED: &str = "SELECT CAST(count(*) AS BIGINT) FROM admin_grants g JOIN accounts a ON a.id = g.account_id JOIN domains d ON d.id = a.domain_id WHERE (CAST($1 AS BIGINT) IS NULL OR g.domain_id = $1) AND (CAST($2 AS TEXT) IS NULL OR strpos(a.local_part || '@' || d.name, CAST($2 AS TEXT)) > 0)";
    pub const COUNT_PLATFORM: &str = "SELECT CAST(count(*) AS BIGINT) FROM admin_grants WHERE domain_id IS NULL";
}
