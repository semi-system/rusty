//! `[dav]`: the calendar and contacts service (docs/design-caldav-carddav.md). Only the dav role has this section. It listens behind the edge, which ends TLS and sends the PROXY header; there is no browser
//! and no cookie, so there is no origin to name: the clients are calendar and contact apps that send a name and a password with every request.

use std::net::SocketAddr;

use serde::Deserialize;

use crate::{Error, Result};

fn d_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8091))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DavConfig {
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    /// The only networks that may connect (the edge): any other peer is dropped before a byte is read. Empty: any peer (development). At most 16 blocks, none that holds every address.
    #[serde(default)]
    pub allowed_peers: Vec<crate::cidr::Cidr>,
    /// DNS names whose addresses may connect as well (the edge by its service name), resolved again every few seconds. At most 8.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// The edge sends the PROXY protocol (version 2) before each connection: the address of a client (the sign-in throttle) is the one in the header, not the edge's.
    #[serde(default)]
    pub proxy_protocol: bool,
}

impl Default for DavConfig {
    fn default() -> DavConfig {
        DavConfig { listen: d_listen(), allowed_peers: Vec::new(), allowed_hosts: Vec::new(), proxy_protocol: false }
    }
}

impl DavConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.allowed_peers.len() > 16 || self.allowed_peers.iter().any(|c| c.is_everything()) {
            return Err(Error::Invalid("dav.allowed_peers must be at most 16 address blocks and none may hold every address (leave it empty to allow any peer)".into()));
        }
        if self.allowed_hosts.len() > 8 || !self.allowed_hosts.iter().all(|h| crate::config_metrics::host_ok(h)) {
            return Err(Error::Invalid("dav.allowed_hosts must be at most 8 DNS names".into()));
        }
        if self.proxy_protocol && self.allowed_peers.is_empty() && self.allowed_hosts.is_empty() {
            return Err(Error::Invalid("dav.proxy_protocol needs allowed_peers or allowed_hosts: the header is believed only from the edge".into()));
        }
        Ok(())
    }
}
