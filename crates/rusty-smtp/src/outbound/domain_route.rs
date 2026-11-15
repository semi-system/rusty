//! `DomainRoute`: the [`OutboundRoute`] that gives a domain its **own relay** when its tenant entered one (admin web design, section 14.4), and
//! otherwise defers to another route (the platform's `[outbound.relay]`, or direct delivery).
//!
//! The relay is read from the database (`domain_relays`), its password opened from `sealed_secrets` with this service's unsealing key, and the
//! result kept for a short time (a changed or removed relay is noticed within the ttl). Three rules make a tenant's choice safe:
//!
//! * the spec is marked `public_only`: the worker refuses to connect to any address that is not public, **checked again at every connection**
//!   (a name that pointed at a public address when it was entered may point inside later);
//! * TLS is the verified kind whatever the tenant says (the worker already maps every mode but `none` to verified TLS);
//! * a relay with a login whose password is missing or cannot be opened is an **error**, never a connection without credentials: the worker
//!   defers the mail and says why.
//!
//! The sender domain picks the relay: mail a tenant's users send leaves through that tenant's relay.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rusty_core::{Error, Result};

use super::route::{Credentials, OutboundRoute, RelaySpec, Route};

/// `sealed_secrets.purpose` of a relay password (owner kind `domain`).
pub const RELAY_PASSWORD_PURPOSE: &str = "relay-password";

/// How long a loaded route is kept.
pub const DOMAIN_ROUTE_TTL: Duration = Duration::from_secs(60);

type Entry = (Instant, Option<RelaySpec>);

pub struct DomainRoute {
    directory: rusty_directory::Directory,
    relays: rusty_store::RelayStore,
    reader: rusty_store::SealedReader,
    unsealer: rusty_seal::Unsealer,
    fallback: Arc<dyn OutboundRoute>,
    ttl: Duration,
    cache: std::sync::Mutex<std::collections::HashMap<String, Entry>>,
}

impl DomainRoute {
    pub fn new(
        directory: rusty_directory::Directory,
        relays: rusty_store::RelayStore,
        reader: rusty_store::SealedReader,
        unsealer: rusty_seal::Unsealer,
        fallback: Arc<dyn OutboundRoute>,
    ) -> DomainRoute {
        DomainRoute { directory, relays, reader, unsealer, fallback, ttl: DOMAIN_ROUTE_TTL, cache: Default::default() }
    }

    /// How long loaded relays are kept (zero: every delivery reads the database).
    pub fn with_ttl(mut self, ttl: Duration) -> DomainRoute {
        self.ttl = ttl;
        self
    }

    async fn load(&self, sender_domain: &str) -> Result<Option<RelaySpec>> {
        let Some(dom) = self.directory.find_domain(sender_domain).await? else { return Ok(None) };
        let Some(row) = self.relays.get(dom.id).await? else { return Ok(None) };
        let tls = rusty_store::tls_of(&row.tls).ok_or_else(|| Error::Invalid("the stored relay has an unknown TLS mode".into()))?;
        let credentials = match row.username {
            None => None,
            Some(username) => {
                let secret = self
                    .reader
                    .get("domain", dom.id.0, RELAY_PASSWORD_PURPOSE)
                    .await?
                    .ok_or_else(|| Error::Invalid("the relay has a user name but no password was set".into()))?;
                let context = rusty_seal::Context::new("domain", dom.id.0, RELAY_PASSWORD_PURPOSE)?;
                let plain = self.unsealer.unseal(&context, &secret.sealed)?;
                let password = String::from_utf8(plain.to_vec()).map_err(|_| Error::Invalid("the relay password is not text".into()))?;
                Some(Credentials { username, password })
            }
        };
        Ok(Some(RelaySpec { host: row.host, port: row.port, tls, credentials, public_only: true }))
    }
}

#[async_trait]
impl OutboundRoute for DomainRoute {
    async fn route(&self, sender_domain: &str, rcpt_domain: &str) -> Result<Route> {
        let key = sender_domain.to_ascii_lowercase();
        let now = Instant::now();
        let cached = if self.ttl > Duration::ZERO {
            self.cache.lock().ok().and_then(|c| c.get(&key).filter(|(at, _)| now.duration_since(*at) < self.ttl).map(|(_, v)| v.clone()))
        } else {
            None
        };
        let spec = match cached {
            Some(s) => s,
            None => {
                let s = self.load(&key).await?;
                if self.ttl > Duration::ZERO {
                    if let Ok(mut c) = self.cache.lock() {
                        c.insert(key, (now, s.clone()));
                    }
                }
                s
            }
        };
        match spec {
            Some(s) => Ok(Route::Relay(s)),
            None => self.fallback.route(sender_domain, rcpt_domain).await,
        }
    }
}
