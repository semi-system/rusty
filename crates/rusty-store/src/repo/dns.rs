//! `DnsRepo`: persistence of the DNS validation state (plan U10, "Persistence for later
//! re-validation"; migrations 0014 and 0015 on both backends).
//!
//! # What is stored (and what is not)
//!
//! * `domain_dns_config`: one row per domain with the inputs of the generated record set (DKIM
//!   selector and algorithm, DMARC policy and `rua`, an outbound identity reference), the SHA-256
//!   of the ownership challenge token and `expected_hash`, the hash of the generated record set
//!   at the last "apply".
//! * `domain_dns_state`: one row per domain and record key `(name, type, purpose)`: the hash of
//!   the expected value and the last observation (status, truncated evidence, timestamps,
//!   consecutive failures).
//! * `domain_dns_event`: the bounded history of status changes.
//!
//! The records themselves are never stored (they are regenerated from configuration and key) and
//! no secret is: the ownership token only as a hash, DKIM only the public selector.
//!
//! # Boundary
//!
//! The pure drift rules live in `rusty_core::dns_state`; [`DnsRepo::record_observations`] applies
//! them inside ONE transaction that first takes the domain row (`FOR NO KEY UPDATE` on Postgres,
//! the single writer on SQLite), so two concurrent checks of a domain serialise and a change is
//! recorded exactly once. The clock is the DATABASE clock (`now: None`); `Some(t)` is for the
//! test twins only.
//!
//! # Tenant isolation
//!
//! Every statement is keyed by `domain_id`; the facade ([`crate::DnsStore`]) takes a
//! [`DomainId`] the caller resolved through the directory. A row cannot outlive its domain
//! (`ON DELETE CASCADE`).

use async_trait::async_trait;
use rusty_core::dns_state::{Observation, RecordKey, RecordStatus, StateRow, Step};
use rusty_core::{DomainId, Result};

/// Per-domain DNS settings (what the generated record set is built from).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DnsConfig {
    pub dkim_selector: Option<String>,
    /// `rsa` or `ed25519`.
    pub dkim_algorithm: Option<String>,
    /// `none`, `quarantine` or `reject`.
    pub dmarc_policy: String,
    pub dmarc_rua: Option<String>,
    /// An address or a relay name; never a credential.
    pub outbound_ref: Option<String>,
}

/// A stored configuration row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsConfigRow {
    pub domain: DomainId,
    pub config: DnsConfig,
    pub has_ownership_token: bool,
    /// Hash of the generated record set at the last apply.
    pub expected_hash: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One status change of the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsEventRow {
    pub id: i64,
    pub key: RecordKey,
    pub from: Option<RecordStatus>,
    pub to: RecordStatus,
    pub at: i64,
}

#[async_trait]
pub trait DnsRepo: Send + Sync {
    /// The database clock in unix seconds (a read).
    async fn db_now(&self) -> Result<i64>;
    /// The configuration row, `None` if the domain has none yet.
    async fn config(&self, domain: DomainId) -> Result<Option<DnsConfigRow>>;
    /// Insert or replace the settings (the token hash and `expected_hash` are kept). `NotFound` if
    /// the domain does not exist.
    async fn put_config(&self, domain: DomainId, config: &DnsConfig, now: Option<i64>) -> Result<()>;
    /// Store the SHA-256 (hex) of a new ownership token, replacing the old one.
    async fn set_ownership_token_hash(&self, domain: DomainId, hash: &str, now: Option<i64>) -> Result<()>;
    /// The stored token hash, if any.
    async fn ownership_token_hash(&self, domain: DomainId) -> Result<Option<String>>;
    /// Remember `hash` as the hash of the record set the admin applied.
    async fn set_expected_hash(&self, domain: DomainId, hash: &str, now: Option<i64>) -> Result<()>;
    /// All record states of the domain, ordered by key.
    async fn states(&self, domain: DomainId) -> Result<Vec<StateRow>>;
    /// Apply one full check: `observations` is the domain's CURRENT record set. Each is run
    /// through `rusty_core::dns_state::step` against its stored row (state upserted, a status
    /// change appended to the history), rows of records that are no longer in the set are
    /// deleted, and the history is cut to the newest `event_cap` rows. One transaction. Returns
    /// the step of every observation, in order.
    async fn record_observations(
        &self,
        domain: DomainId,
        observations: &[Observation],
        alert_after: u32,
        event_cap: u32,
        now: Option<i64>,
    ) -> Result<Vec<Step>>;
    /// History rows with `at >= since`, newest first, at most `limit`.
    async fn events(&self, domain: DomainId, since: i64, limit: u32) -> Result<Vec<DnsEventRow>>;
}

/// Statement text shared by both backends (`{NOW}` = database clock, `$N` placeholders).
pub(crate) mod sql {
    pub const CONFIG: &str = "SELECT dkim_selector, dkim_algorithm, dmarc_policy, dmarc_rua, outbound_ref, \
        CAST(CASE WHEN ownership_token_hash IS NULL THEN 0 ELSE 1 END AS BIGINT) AS has_token, \
        expected_hash, created_at, updated_at FROM domain_dns_config WHERE domain_id = $1";
    pub const PUT_CONFIG: &str = "INSERT INTO domain_dns_config (domain_id, dkim_selector, dkim_algorithm, \
        dmarc_policy, dmarc_rua, outbound_ref, created_at, updated_at) \
        VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, {NOW}), COALESCE($7, {NOW})) \
        ON CONFLICT (domain_id) DO UPDATE SET dkim_selector = excluded.dkim_selector, \
        dkim_algorithm = excluded.dkim_algorithm, dmarc_policy = excluded.dmarc_policy, \
        dmarc_rua = excluded.dmarc_rua, outbound_ref = excluded.outbound_ref, updated_at = excluded.updated_at";
    pub const SET_TOKEN: &str = "INSERT INTO domain_dns_config (domain_id, ownership_token_hash, created_at, updated_at) \
        VALUES ($1, $2, COALESCE($3, {NOW}), COALESCE($3, {NOW})) \
        ON CONFLICT (domain_id) DO UPDATE SET ownership_token_hash = excluded.ownership_token_hash, \
        updated_at = excluded.updated_at";
    pub const GET_TOKEN: &str = "SELECT ownership_token_hash FROM domain_dns_config WHERE domain_id = $1";
    pub const SET_EXPECTED: &str = "INSERT INTO domain_dns_config (domain_id, expected_hash, created_at, updated_at) \
        VALUES ($1, $2, COALESCE($3, {NOW}), COALESCE($3, {NOW})) \
        ON CONFLICT (domain_id) DO UPDATE SET expected_hash = excluded.expected_hash, \
        updated_at = excluded.updated_at";
    pub const STATES: &str = "SELECT record_name, record_type, purpose, expected_value_hash, last_status, \
        last_seen_value, first_ok_at, last_ok_at, last_checked_at, status_since, consecutive_failures \
        FROM domain_dns_state WHERE domain_id = $1 ORDER BY record_name, record_type, purpose";
    pub const UPSERT_STATE: &str = "INSERT INTO domain_dns_state (domain_id, record_name, record_type, purpose, \
        expected_value_hash, last_status, last_seen_value, first_ok_at, last_ok_at, last_checked_at, \
        status_since, consecutive_failures) \
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
        ON CONFLICT (domain_id, record_name, record_type, purpose) DO UPDATE SET \
        expected_value_hash = excluded.expected_value_hash, last_status = excluded.last_status, \
        last_seen_value = excluded.last_seen_value, first_ok_at = excluded.first_ok_at, \
        last_ok_at = excluded.last_ok_at, last_checked_at = excluded.last_checked_at, \
        status_since = excluded.status_since, consecutive_failures = excluded.consecutive_failures";
    pub const DELETE_STATE: &str = "DELETE FROM domain_dns_state \
        WHERE domain_id = $1 AND record_name = $2 AND record_type = $3 AND purpose = $4";
    pub const INSERT_EVENT: &str = "INSERT INTO domain_dns_event (domain_id, record_name, record_type, purpose, \
        from_status, to_status, at) VALUES ($1, $2, $3, $4, $5, $6, $7)";
    /// Keep the newest `$2` rows of the domain: delete everything at or below the id of the
    /// `$2`-th newest (no row at that offset: the subquery is NULL and nothing is deleted).
    pub const TRIM_EVENTS: &str = "DELETE FROM domain_dns_event WHERE domain_id = $1 AND id <= \
        (SELECT id FROM domain_dns_event WHERE domain_id = $1 ORDER BY id DESC LIMIT 1 OFFSET $2)";
    pub const EVENTS: &str = "SELECT id, record_name, record_type, purpose, from_status, to_status, at \
        FROM domain_dns_event WHERE domain_id = $1 AND at >= $2 ORDER BY id DESC LIMIT $3";
}

/// Decode a stored row (the columns of [`sql::STATES`]) into a [`StateRow`]. A row whose status
/// is unknown (cannot happen under the CHECK constraints) is an error, not a guess.
pub(crate) struct RawState {
    pub name: String,
    pub rtype: String,
    pub purpose: String,
    pub expected: String,
    pub status: String,
    pub seen: Option<String>,
    pub first_ok_at: Option<i64>,
    pub last_ok_at: Option<i64>,
    pub last_checked_at: i64,
    pub status_since: i64,
    pub failures: i64,
}

impl RawState {
    pub fn into_row(self) -> Result<StateRow> {
        let status = RecordStatus::parse(&self.status)
            .ok_or_else(|| rusty_core::Error::storage("unreadable DNS state row"))?;
        Ok(StateRow {
            key: RecordKey { name: self.name, rtype: self.rtype, purpose: self.purpose },
            expected_hash: self.expected,
            status,
            seen: self.seen,
            first_ok_at: self.first_ok_at,
            last_ok_at: self.last_ok_at,
            last_checked_at: self.last_checked_at,
            status_since: self.status_since,
            consecutive_failures: u32::try_from(self.failures.max(0)).unwrap_or(u32::MAX),
        })
    }
}

pub(crate) struct RawEvent {
    pub id: i64,
    pub name: String,
    pub rtype: String,
    pub purpose: String,
    pub from: Option<String>,
    pub to: String,
    pub at: i64,
}

impl RawEvent {
    pub fn into_row(self) -> Result<DnsEventRow> {
        let bad = || rusty_core::Error::storage("unreadable DNS event row");
        Ok(DnsEventRow {
            id: self.id,
            key: RecordKey { name: self.name, rtype: self.rtype, purpose: self.purpose },
            from: match self.from {
                Some(s) => Some(RecordStatus::parse(&s).ok_or_else(bad)?),
                None => None,
            },
            to: RecordStatus::parse(&self.to).ok_or_else(bad)?,
            at: self.at,
        })
    }
}

/// Reasons a check refuses to write: the observation set is bounded, and every field the
/// database constrains is validated here first so the error is `Invalid`, not a constraint text.
pub(crate) fn validate_observations(obs: &[Observation]) -> Result<()> {
    use rusty_core::Error;
    if obs.len() > crate::dns::MAX_RECORDS_PER_DOMAIN {
        return Err(Error::Invalid("too many DNS records".into()));
    }
    let mut seen = std::collections::HashSet::new();
    for o in obs {
        let k = &o.key;
        if !(1..=255).contains(&k.name.len())
            || !(1..=128).contains(&k.purpose.len())
            || !matches!(k.rtype.as_str(), "MX" | "A" | "AAAA" | "TXT" | "SRV" | "PTR" | "CNAME")
            || o.expected_hash.len() != 64
            || !o.expected_hash.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(Error::Invalid("invalid DNS record observation".into()));
        }
        if !seen.insert(k) {
            return Err(Error::Invalid("duplicate DNS record observation".into()));
        }
    }
    Ok(())
}

/// What one check changes in the database.
pub(crate) struct Plan {
    pub steps: Vec<Step>,
    /// Keys stored but no longer in the expected set.
    pub delete: Vec<RecordKey>,
}

/// The pure part of [`DnsRepo::record_observations`], shared by both backends: run every
/// observation through the drift rules against the stored rows read under the domain lock.
pub(crate) fn plan(existing: Vec<StateRow>, obs: &[Observation], alert_after: u32, now: i64) -> Plan {
    let mut by_key: std::collections::HashMap<RecordKey, StateRow> =
        existing.into_iter().map(|r| (r.key.clone(), r)).collect();
    let steps: Vec<Step> = obs
        .iter()
        .map(|o| rusty_core::dns_state::step(by_key.remove(&o.key).as_ref(), o, now, alert_after))
        .collect();
    // What is left was stored but is not in the new set.
    let mut delete: Vec<RecordKey> = by_key.into_keys().collect();
    delete.sort();
    Plan { steps, delete }
}
