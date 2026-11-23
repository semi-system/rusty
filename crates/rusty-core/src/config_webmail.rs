//! `[webmail]`: the webmail service (docs/design-webmail.md). Only the webmail role has this section. It is the admin web's `[admin]` for the people who own a mailbox: where it listens (behind the edge,
//! which ends TLS), the origin the browser writes in `Origin`, and who may connect.

use std::net::SocketAddr;

use serde::Deserialize;

use crate::config_admin::valid_origin;
use crate::{Error, Result};

fn d_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8090))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebmailConfig {
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    /// `https://host[:port]` of the webmail host name, exactly as the browser writes it in `Origin` (lower case, no path).
    pub public_origin: String,
    /// The only networks that may connect (the edge): any other peer is dropped before a byte is read. Empty: any peer (development). At most 16 blocks, none that holds every address.
    #[serde(default)]
    pub allowed_peers: Vec<crate::cidr::Cidr>,
    /// DNS names whose addresses may connect as well (the edge by its service name), resolved again every few seconds. At most 8.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// The edge sends the PROXY protocol (version 2) before each connection: the address of a client (the sign-in throttle) is the one in the header, not the edge's. Needs `allowed_peers` or `allowed_hosts`.
    #[serde(default)]
    pub proxy_protocol: bool,
    /// More origins the webmail is reached by (another port, a tenant's own host name). At most 4, each as `public_origin`.
    #[serde(default)]
    pub extra_origins: Vec<String>,
}

impl WebmailConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !valid_origin(&self.public_origin) {
            return Err(Error::Invalid("webmail.public_origin must be https://host or https://host:port in lower case, with no path".into()));
        }
        if self.allowed_peers.len() > 16 || self.allowed_peers.iter().any(|c| c.is_everything()) {
            return Err(Error::Invalid("webmail.allowed_peers must be at most 16 address blocks and none may hold every address (leave it empty to allow any peer)".into()));
        }
        if self.allowed_hosts.len() > 8 || !self.allowed_hosts.iter().all(|h| crate::config_metrics::host_ok(h)) {
            return Err(Error::Invalid("webmail.allowed_hosts must be at most 8 DNS names".into()));
        }
        if self.extra_origins.len() > 4 || !self.extra_origins.iter().all(|o| valid_origin(o)) {
            return Err(Error::Invalid("webmail.extra_origins must be at most 4 origins, each https://host or https://host:port in lower case, with no path".into()));
        }
        if self.proxy_protocol && self.allowed_peers.is_empty() && self.allowed_hosts.is_empty() {
            return Err(Error::Invalid("webmail.proxy_protocol needs allowed_peers or allowed_hosts: the header is believed only from the edge".into()));
        }
        Ok(())
    }
}
