//! `DnsZoneRepo`: what the authoritative name server and the admin web need of hosted zones (migration 0055, docs/design-authoritative-dns.md). The name server reads `hosted()`; the admin side sets the mode of a
//! domain and edits the extra records. A query never changes anything.

use async_trait::async_trait;
use rusty_core::{DomainId, Result};

/// The most extra records one domain may have.
pub const MAX_EXTRA_RECORDS: i64 = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraRecord {
    pub id: i64,
    /// Relative to the domain; `@` is the domain itself.
    pub name: String,
    pub rtype: String,
    pub value: String,
    pub ttl: u32,
}

/// What the name server needs to build the zone of one hosted domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedDomain {
    pub name: String,
    /// `(selector, algorithm, public key)` of the DKIM keys: public data only.
    pub dkim: Vec<(String, String, String)>,
    /// The SPF include of the relay the domain sends through, if any.
    pub spf_include: Option<String>,
    pub extra: Vec<ExtraRecord>,
}

#[async_trait]
pub trait DnsZoneRepo: Send + Sync {
    /// Every active domain in `hosted` mode, with what its zone needs.
    async fn hosted(&self) -> Result<Vec<HostedDomain>>;
    /// `external` or `hosted`.
    async fn set_mode(&self, domain: DomainId, mode: &str) -> Result<()>;
    async fn mode(&self, domain: DomainId) -> Result<String>;
    async fn extras(&self, domain: DomainId) -> Result<Vec<ExtraRecord>>;
    /// Add one (`Error::Quota` past [`MAX_EXTRA_RECORDS`]); the caller has validated it with `validate_extra`.
    async fn add_extra(&self, domain: DomainId, name: &str, rtype: &str, value: &str, ttl: u32) -> Result<i64>;
    /// Remove one of this domain's; whether there was one.
    async fn remove_extra(&self, domain: DomainId, id: i64) -> Result<bool>;
}

pub(crate) mod sql {
    pub const HOSTED: &str = "SELECT d.id, d.name, r.spf_include FROM domains d LEFT JOIN domain_relays r ON r.domain_id = d.id WHERE d.dns_mode = 'hosted' AND d.status = 'active' ORDER BY d.name";
    pub const KEYS: &str = "SELECT domain_id, selector, algorithm, public_key_b64 FROM domain_dkim_keys WHERE domain_id = ANY($1) ORDER BY domain_id, selector";
    pub const EXTRAS_OF: &str = "SELECT id, name, rtype, value, ttl FROM domain_dns_extra WHERE domain_id = $1 ORDER BY id";
    pub const EXTRAS_OF_MANY: &str = "SELECT domain_id, id, name, rtype, value, ttl FROM domain_dns_extra WHERE domain_id = ANY($1) ORDER BY domain_id, id";
    pub const SET_MODE: &str = "UPDATE domains SET dns_mode = $2 WHERE id = $1";
    pub const MODE: &str = "SELECT dns_mode FROM domains WHERE id = $1";
    pub const COUNT: &str = "SELECT count(*) FROM domain_dns_extra WHERE domain_id = $1";
    pub const ADD: &str = "INSERT INTO domain_dns_extra (domain_id, name, rtype, value, ttl, created_at) VALUES ($1, $2, $3, $4, $5, rs_now()) RETURNING id";
    pub const REMOVE: &str = "DELETE FROM domain_dns_extra WHERE domain_id = $1 AND id = $2";
}
