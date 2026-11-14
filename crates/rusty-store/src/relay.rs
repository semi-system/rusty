//! `RelayStore`: the facade over [`RelayRepo`] for the per-domain relay (admin web design, section 14.4). It validates with
//! `rusty_core::relay_policy` before the database sees anything: a tenant chooses where the outbound service connects, so the host, port and
//! TLS mode are checked here (and again by the table's constraints). The password is not handled here; it is a sealed secret.

use std::sync::Arc;

use rusty_core::config::RelayTls;
use rusty_core::relay_policy::{validate_tenant_relay, TenantRelay};
use rusty_core::{DomainId, Error, Result};

use crate::repo::{RelayRepo, RelayRow};

/// The fixed categories a connection check may record. Nothing else is stored: a server's reply text never reaches the database.
pub const CHECK_STATUSES: [&str; 7] = ["ok", "auth_failed", "tls_failed", "unreachable", "refused_address", "no_login_mechanism", "unsealable"];

#[derive(Clone)]
pub struct RelayStore {
    repo: Arc<dyn RelayRepo>,
}

/// The `tls` column text of a mode.
pub fn tls_text(tls: RelayTls) -> &'static str {
    match tls {
        RelayTls::Starttls => "starttls",
        RelayTls::Implicit => "implicit",
        RelayTls::None => "none",
    }
}

/// The mode of a stored `tls` text.
pub fn tls_of(text: &str) -> Option<RelayTls> {
    match text {
        "starttls" => Some(RelayTls::Starttls),
        "implicit" => Some(RelayTls::Implicit),
        "none" => Some(RelayTls::None),
        _ => None,
    }
}

impl RelayStore {
    pub fn with_repo(repo: Arc<dyn RelayRepo>) -> RelayStore {
        RelayStore { repo }
    }

    /// Set the relay of a domain (replacing any, and forgetting the last check). `spf_include` is the provider's `include:` name.
    pub async fn set(&self, domain: DomainId, relay: &TenantRelay, spf_include: Option<&str>) -> Result<RelayRow> {
        self.set_inner(domain, relay, spf_include, None).await
    }

    /// [`set`](Self::set) at a given time (tests).
    #[doc(hidden)]
    pub async fn set_at(&self, domain: DomainId, relay: &TenantRelay, spf_include: Option<&str>, now: i64) -> Result<RelayRow> {
        self.set_inner(domain, relay, spf_include, Some(now)).await
    }

    async fn set_inner(&self, domain: DomainId, relay: &TenantRelay, spf_include: Option<&str>, now: Option<i64>) -> Result<RelayRow> {
        // Validate again from the parts: a TenantRelay built by hand is not trusted.
        let ok = validate_tenant_relay(&relay.host, relay.port, relay.tls, relay.username.as_deref())?;
        if let Some(s) = spf_include {
            if !rusty_core::dns::valid_spf_domain(s) {
                return Err(Error::Invalid("the SPF include must be a DNS name".into()));
            }
        }
        let row = RelayRow {
            domain,
            host: ok.host,
            port: ok.port,
            tls: tls_text(ok.tls).to_owned(),
            username: ok.username,
            spf_include: spf_include.map(str::to_owned),
            check_status: None,
            checked_at: None,
            created_at: 0,
            updated_at: 0,
        };
        self.repo.put(&row, now).await
    }

    pub async fn get(&self, domain: DomainId) -> Result<Option<RelayRow>> {
        self.repo.get(domain).await
    }

    /// Remove the relay of a domain; whether there was one. (Its sealed password is removed by the caller through the sealed facade.)
    pub async fn remove(&self, domain: DomainId) -> Result<bool> {
        self.repo.delete(domain).await
    }

    /// Record the outcome of a connection check; `status` is one of [`CHECK_STATUSES`]. `false` when the domain has no relay.
    pub async fn record_check(&self, domain: DomainId, status: &str) -> Result<bool> {
        self.record_check_inner(domain, status, None).await
    }

    #[doc(hidden)]
    pub async fn record_check_at(&self, domain: DomainId, status: &str, now: i64) -> Result<bool> {
        self.record_check_inner(domain, status, Some(now)).await
    }

    async fn record_check_inner(&self, domain: DomainId, status: &str, now: Option<i64>) -> Result<bool> {
        if !CHECK_STATUSES.contains(&status) {
            return Err(Error::Invalid("unknown check status".into()));
        }
        self.repo.record_check(domain, status, now).await
    }
}
