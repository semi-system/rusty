//! `OutboundRoute`: the seam that decides HOW mail for a recipient domain leaves (plan section 1b,
//! seam table): directly to the recipient's exchangers, from which local address, or through a relay
//! (smarthost) with credentials. The global-configuration implementation is [`ConfigRoute`]; per-tenant
//! relays or source IPs are another implementation of the trait, chosen by the composition root, with no
//! change to the worker.

use std::net::IpAddr;
use std::path::Path;

use async_trait::async_trait;
use rusty_core::config::{OutboundConfig, RelayTls};
use rusty_core::{Error, Result};
use rusty_svc::SecretSource;

/// Relay login. The password is never printed (`Debug` redacts it).
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("username", &self.username).field("password", &"<redacted>").finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaySpec {
    pub host: String,
    pub port: u16,
    pub tls: RelayTls,
    pub credentials: Option<Credentials>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Resolve the recipient's MX and deliver there, from `bind` when set (dedicated sending IP).
    Direct { bind: Option<IpAddr> },
    /// Hand everything to this relay.
    Relay(RelaySpec),
}

#[async_trait]
pub trait OutboundRoute: Send + Sync {
    async fn route(&self, sender_domain: &str, rcpt_domain: &str) -> Result<Route>;
}

/// The routing of the `[outbound]` section: one global behaviour for every tenant.
pub struct ConfigRoute {
    route: Route,
}

impl ConfigRoute {
    /// Reads the relay password through `secrets` (mode 0600 enforced) once, at start-up: a bad file
    /// stops the worker from starting instead of failing every delivery later.
    pub fn from_config(cfg: &OutboundConfig, secrets: &dyn SecretSource) -> Result<ConfigRoute> {
        let route = match &cfg.relay {
            None => Route::Direct { bind: cfg.bind_address },
            Some(r) => {
                let credentials = match (&r.username, &r.password_file) {
                    (Some(u), Some(p)) => Some(Credentials { username: u.clone(), password: read_password(secrets, p)? }),
                    _ => None,
                };
                Route::Relay(RelaySpec { host: r.host.clone(), port: r.port, tls: r.tls, credentials })
            }
        };
        Ok(ConfigRoute { route })
    }
}

fn read_password(secrets: &dyn SecretSource, path: &Path) -> Result<String> {
    let bytes = secrets.read(path)?;
    let text = String::from_utf8(bytes).map_err(|_| Error::Invalid("the relay password file is not UTF-8".into()))?;
    let pw = text.trim_end_matches(['\n', '\r']).to_owned();
    if pw.is_empty() || pw.chars().any(char::is_control) {
        return Err(Error::Invalid("the relay password file must hold one line".into()));
    }
    Ok(pw)
}

#[async_trait]
impl OutboundRoute for ConfigRoute {
    async fn route(&self, _sender_domain: &str, _rcpt_domain: &str) -> Result<Route> {
        Ok(self.route.clone())
    }
}
