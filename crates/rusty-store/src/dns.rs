//! `DnsStore`: the facade over [`DnsRepo`] for the DNS validation state (plan U10). It keeps what
//! is not a database concern: validation of every settable field, the ownership token (random,
//! stored only as a SHA-256, compared in constant time), the history cap and the bounds.
//! Service crates use this type and never SQL.

use std::fmt;
use std::sync::Arc;

use rand_core::{OsRng, RngCore};
use rusty_core::dns_state::{Observation, StateRow, Step};
use rusty_core::{DomainId, Error, Result};
use sha2::{Digest, Sha256};

use crate::repo::{DnsConfig, DnsConfigRow, DnsEventRow, DnsRepo};

/// Most records one domain's expected set may have (the generator makes about a dozen).
pub const MAX_RECORDS_PER_DOMAIN: usize = 64;
/// Status changes kept per domain; older ones are removed by the write that adds a newer one.
pub const DNS_EVENTS_PER_DOMAIN: u32 = 200;
/// Most history rows one [`DnsStore::events`] call returns.
pub const MAX_DNS_EVENT_PAGE: u32 = 200;
/// Most characters of the token the challenge record carries (64 hex digits = 256 bits).
const TOKEN_BYTES: usize = 32;

fn invalid(what: &'static str) -> Error {
    Error::Invalid(what.into())
}

/// A freshly generated ownership token. Shown to the admin ONCE (it goes into the challenge TXT
/// record); only its hash is stored. Never printed by `Debug`.
pub struct OwnershipToken(String);

impl OwnershipToken {
    /// The token text (64 lower-case hex digits) for the challenge record.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OwnershipToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OwnershipToken(<redacted>)")
    }
}

/// Checks a presented token against the stored hash, in constant time. Holds the hash only.
#[derive(Clone)]
pub struct TokenVerifier {
    hash: [u8; 32],
}

impl fmt::Debug for TokenVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenVerifier(<redacted>)")
    }
}

fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.trim().to_ascii_lowercase().as_bytes()).into()
}

/// Constant-time equality of two equal-length digests: no early exit on the first difference.
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

impl TokenVerifier {
    /// The presented text (a token found in a TXT record) is the stored token.
    pub fn accepts(&self, presented: &str) -> bool {
        // Hash first: the compared values have the same length whatever was presented.
        ct_eq(&hash_token(presented), &self.hash)
    }

    fn from_hex(h: &str) -> Option<TokenVerifier> {
        let bytes = hex::decode(h).ok()?;
        Some(TokenVerifier { hash: bytes.try_into().ok()? })
    }
}

fn valid_label(s: &str) -> bool {
    (1..=63).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && !s.starts_with(['-', '.'])
        && !s.ends_with(['-', '.'])
}

fn valid_text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}

/// The DNS validation store over any backend's [`DnsRepo`].
#[derive(Clone)]
pub struct DnsStore {
    repo: Arc<dyn DnsRepo>,
    event_cap: u32,
}

impl DnsStore {
    pub fn with_repo(repo: Arc<dyn DnsRepo>) -> DnsStore {
        DnsStore { repo, event_cap: DNS_EVENTS_PER_DOMAIN }
    }

    /// A store with a smaller history cap (for tests; production uses [`with_repo`](Self::with_repo)).
    #[doc(hidden)]
    pub fn with_event_cap(repo: Arc<dyn DnsRepo>, event_cap: u32) -> DnsStore {
        DnsStore { repo, event_cap }
    }

    pub async fn config(&self, domain: DomainId) -> Result<Option<DnsConfigRow>> {
        self.repo.config(domain).await
    }

    /// Insert or replace the settings of a domain. `NotFound` if the domain does not exist,
    /// `Invalid` for a value that could not be published.
    pub async fn put_config(&self, domain: DomainId, config: &DnsConfig) -> Result<()> {
        self.put_config_inner(domain, config, None).await
    }

    #[doc(hidden)]
    pub async fn put_config_at(&self, domain: DomainId, config: &DnsConfig, now: i64) -> Result<()> {
        crate::mailbox::check_supplied_time(now)?;
        self.put_config_inner(domain, config, Some(now)).await
    }

    async fn put_config_inner(&self, domain: DomainId, c: &DnsConfig, now: Option<i64>) -> Result<()> {
        if c.dkim_selector.as_deref().is_some_and(|s| !valid_label(s)) {
            return Err(invalid("invalid DKIM selector"));
        }
        if c.dkim_algorithm.as_deref().is_some_and(|a| !matches!(a, "rsa" | "ed25519")) {
            return Err(invalid("invalid DKIM algorithm"));
        }
        if c.dkim_selector.is_some() != c.dkim_algorithm.is_some() {
            return Err(invalid("DKIM selector and algorithm are set together"));
        }
        if !matches!(c.dmarc_policy.as_str(), "none" | "quarantine" | "reject") {
            return Err(invalid("invalid DMARC policy"));
        }
        if c.dmarc_rua.as_deref().is_some_and(|r| !(3..=254).contains(&r.len()) || !r.contains('@') || r.chars().any(|c| c.is_control() || c.is_whitespace() || c == ';' || c == ',')) {
            return Err(invalid("invalid DMARC report address"));
        }
        if c.outbound_ref.as_deref().is_some_and(|o| !valid_text(o, 255)) {
            return Err(invalid("invalid outbound reference"));
        }
        self.repo.put_config(domain, c, now).await
    }

    /// Generate a new ownership token for the domain (replacing the old one) and store its hash.
    /// The returned token is the only copy.
    pub async fn new_ownership_token(&self, domain: DomainId) -> Result<OwnershipToken> {
        let mut raw = [0u8; TOKEN_BYTES];
        OsRng.fill_bytes(&mut raw);
        let token = hex::encode(raw);
        let hash = hex::encode(hash_token(&token));
        self.repo.set_ownership_token_hash(domain, &hash, None).await?;
        Ok(OwnershipToken(token))
    }

    /// The verifier for the domain's stored token, `None` if no token was generated. An unreadable
    /// stored hash is a storage error, not "no token" (it would silently disable the gate).
    pub async fn ownership_verifier(&self, domain: DomainId) -> Result<Option<TokenVerifier>> {
        match self.repo.ownership_token_hash(domain).await? {
            None => Ok(None),
            Some(h) => TokenVerifier::from_hex(&h)
                .map(Some)
                .ok_or_else(|| Error::storage("unreadable ownership token hash")),
        }
    }

    /// `presented` is the domain's ownership token (constant-time comparison).
    pub async fn verify_ownership(&self, domain: DomainId, presented: &str) -> Result<bool> {
        Ok(self.ownership_verifier(domain).await?.is_some_and(|v| v.accepts(presented)))
    }

    /// The generated record set (as `current_hash`) differs from the one the admin last applied:
    /// the page shows "records changed, update your DNS". `false` before the first apply.
    pub async fn records_changed(&self, domain: DomainId, current_hash: &str) -> Result<bool> {
        Ok(self
            .repo
            .config(domain)
            .await?
            .and_then(|c| c.expected_hash)
            .is_some_and(|h| h != current_hash))
    }

    /// Remember `current_hash` as the record set the admin applied (publishing the records).
    pub async fn acknowledge(&self, domain: DomainId, current_hash: &str) -> Result<()> {
        if current_hash.len() != 64 || !current_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("invalid record set hash"));
        }
        self.repo.set_expected_hash(domain, current_hash, None).await
    }

    pub async fn states(&self, domain: DomainId) -> Result<Vec<StateRow>> {
        self.repo.states(domain).await
    }

    /// Record one full check of the domain: `observations` is its current expected set. The
    /// returned steps carry the status changes and alert changes to log (`dns.drift`).
    pub async fn record_check(&self, domain: DomainId, observations: &[Observation], alert_after: u32) -> Result<Vec<Step>> {
        self.repo
            .record_observations(domain, observations, alert_after.max(1), self.event_cap, None)
            .await
    }

    /// [`record_check`](Self::record_check) at time `now`.
    #[doc(hidden)]
    pub async fn record_check_at(
        &self,
        domain: DomainId,
        observations: &[Observation],
        alert_after: u32,
        now: i64,
    ) -> Result<Vec<Step>> {
        crate::mailbox::check_supplied_time(now)?;
        self.repo
            .record_observations(domain, observations, alert_after.max(1), self.event_cap, Some(now))
            .await
    }

    /// Status changes with `at >= since`, newest first (`since` of 0: all), at most `limit`
    /// (bounded by [`MAX_DNS_EVENT_PAGE`]).
    pub async fn events(&self, domain: DomainId, since: i64, limit: u32) -> Result<Vec<DnsEventRow>> {
        self.repo.events(domain, since.max(0), limit.clamp(1, MAX_DNS_EVENT_PAGE)).await
    }

    /// The database clock in unix seconds.
    pub async fn now(&self) -> Result<i64> {
        self.repo.db_now().await
    }
}
