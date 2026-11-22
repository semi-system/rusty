//! `RoutingRepo`: what the admin web reads and writes about where a domain's mail goes besides its accounts: distribution lists with their members, the catch-all and the domain aliases
//! (docs/design-routing.md). The resolution of an address (`DirectoryRepo::resolve`) uses the same rows.

use async_trait::async_trait;
use rusty_core::{DomainId, ListId, Result};

/// A distribution list with the number of its members.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListInfo {
    pub id: ListId,
    pub local: String,
    pub members: i64,
}

/// One member of a list: the local account it is (its address) or the outside address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberInfo {
    pub seq: i64,
    pub address: String,
    pub external: bool,
}

#[async_trait]
pub trait RoutingRepo: Send + Sync {
    /// The lists of a domain by name, at most 500.
    async fn lists(&self, domain: DomainId) -> Result<Vec<ListInfo>>;
    /// `None`: no such list in this domain.
    async fn find_list(&self, domain: DomainId, local: &str) -> Result<Option<ListInfo>>;
    /// Delete a list and its members (`NotFound` when it does not exist).
    async fn delete_list(&self, domain: DomainId, local: &str) -> Result<()>;
    /// The members of a list in the order they were added.
    async fn members(&self, list: ListId) -> Result<Vec<MemberInfo>>;
    /// Remove the member with this number (`NotFound` when there is none).
    async fn remove_member(&self, list: ListId, seq: i64) -> Result<()>;
    /// The local part that receives the mail of unknown addresses, if one is set.
    async fn catch_all(&self, domain: DomainId) -> Result<Option<String>>;
    /// Set it, or clear it with `None`. The facade has checked that the address exists in the domain.
    async fn set_catch_all(&self, domain: DomainId, local: Option<&str>) -> Result<()>;
    /// The alias domains of a domain by name.
    async fn domain_aliases(&self, domain: DomainId) -> Result<Vec<String>>;
    /// `Conflict` when the name is already an alias or a hosted domain.
    async fn add_domain_alias(&self, domain: DomainId, name: &str) -> Result<()>;
    /// `NotFound` when this domain has no such alias.
    async fn remove_domain_alias(&self, domain: DomainId, name: &str) -> Result<()>;
}

pub(crate) mod sql {
    pub const LISTS: &str = "SELECT l.id, l.local_part, (SELECT count(*) FROM list_members m WHERE m.list_id = l.id) FROM lists l WHERE l.domain_id = $1 ORDER BY l.local_part LIMIT 500";
    pub const FIND_LIST: &str = "SELECT l.id, l.local_part, (SELECT count(*) FROM list_members m WHERE m.list_id = l.id) FROM lists l WHERE l.domain_id = $1 AND l.local_part = $2";
    pub const DELETE_LIST: &str = "DELETE FROM lists WHERE domain_id = $1 AND local_part = $2";
    pub const MEMBERS: &str = "SELECT m.seq, COALESCE(a.local_part || '@' || d.name, m.external), m.account_id IS NULL FROM list_members m \
        LEFT JOIN accounts a ON a.id = m.account_id LEFT JOIN domains d ON d.id = a.domain_id WHERE m.list_id = $1 ORDER BY m.seq";
    pub const REMOVE_MEMBER: &str = "DELETE FROM list_members WHERE list_id = $1 AND seq = $2";
    pub const CATCH_ALL: &str = "SELECT config->>'catch_all' FROM domains WHERE id = $1";
    pub const SET_CATCH_ALL: &str = "UPDATE domains SET config = CASE WHEN CAST($2 AS TEXT) IS NULL THEN config - 'catch_all' ELSE jsonb_set(config, '{catch_all}', to_jsonb(CAST($2 AS TEXT))) END WHERE id = $1";
    pub const DOMAIN_ALIASES: &str = "SELECT name FROM domain_aliases WHERE domain_id = $1 ORDER BY name";
    pub const ADD_DOMAIN_ALIAS: &str = "INSERT INTO domain_aliases (name, domain_id) SELECT $2, $1 WHERE NOT EXISTS (SELECT 1 FROM domains WHERE name = $2) ON CONFLICT DO NOTHING";
    pub const REMOVE_DOMAIN_ALIAS: &str = "DELETE FROM domain_aliases WHERE domain_id = $1 AND name = $2";
}
