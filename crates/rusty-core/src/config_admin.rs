//! `[admin]`: the admin web service (admin web design, section 11). Only the admin role has this section.
//!
//! * `listen`: where the service accepts connections (behind the edge, which terminates TLS and applies the IP allow-list of the admin host name; or
//!   directly with `[tls]`). The default is loopback.
//! * `public_origin`: `https://host[:port]`, exactly what the browser sends in the `Origin` header of a state-changing request. It is compared byte for byte
//!   (scheme, host and port), so it must be written the way the browser writes it: lower case, no trailing slash, no path.

use std::net::{IpAddr, SocketAddr};

use serde::Deserialize;

use crate::{Error, Result};

fn d_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    pub public_origin: String,
    /// The addresses mail leaves from, for the DNS page (the same as `[outbound] public_ips` of the outbound service). Optional: without them the page says so.
    #[serde(default)]
    pub public_ips: Vec<IpAddr>,
    /// The relay provider's SPF `include:` name (as `[outbound.relay] spf_include`), when mail leaves through a relay.
    #[serde(default)]
    pub spf_include: Option<String>,
    /// Mail leaves through a relay: the reverse records of our own addresses are not our business.
    #[serde(default)]
    pub via_relay: bool,
    /// The only networks that may connect (the edge, for a service that sits behind it): any other peer is dropped before a byte is read. Empty: any peer (development).
    /// At most 16 blocks, never one that holds every address.
    #[serde(default)]
    pub allowed_peers: Vec<crate::cidr::Cidr>,
    /// DNS names whose addresses may connect as well (the edge, found by its service name: `edge` in compose, `tasks.edge` in a swarm), resolved again every few seconds. At most 8.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

impl AdminConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !valid_origin(&self.public_origin) {
            return Err(Error::Invalid("admin.public_origin must be https://host or https://host:port in lower case, with no path".into()));
        }
        if self.public_ips.len() > 8 || self.public_ips.iter().enumerate().any(|(i, a)| !crate::dns::publishable_address(*a) || self.public_ips.iter().take(i).any(|o| o == a)) {
            return Err(Error::Invalid("admin.public_ips must be at most 8 distinct addresses that can be published (no loopback, link-local or multicast)".into()));
        }
        if self.spf_include.as_deref().is_some_and(|s| !crate::dns::valid_spf_domain(s)) {
            return Err(Error::Invalid("admin.spf_include must be a DNS name".into()));
        }
        if self.allowed_peers.len() > 16 || self.allowed_peers.iter().any(|c| c.is_everything()) {
            return Err(Error::Invalid("admin.allowed_peers must be at most 16 address blocks and none may hold every address (leave it empty to allow any peer)".into()));
        }
        if self.allowed_hosts.len() > 8 || !self.allowed_hosts.iter().all(|h| crate::config_metrics::host_ok(h)) {
            return Err(Error::Invalid("admin.allowed_hosts must be at most 8 DNS names".into()));
        }
        Ok(())
    }
}

/// `https://` and a lower-case host name or IP literal, optionally `:port`, nothing else.
pub fn valid_origin(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("https://") else { return false };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(':') && !h.is_empty() && !p.is_empty() => (h, Some(p)),
        _ => (rest, None),
    };
    if let Some(p) = port {
        if p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u16>().map_or(true, |n| n == 0) || p.starts_with('0') {
            return false;
        }
    }
    let host_ok = host.len() <= 253
        && !host.is_empty()
        && host.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
    host_ok
}

#[cfg(test)]
mod tests {
    use super::{valid_origin, AdminConfig};

    fn parse(extra: &str) -> Result<AdminConfig, String> {
        let c: AdminConfig = toml::from_str(&format!("public_origin = \"https://admin.example.org\"
{extra}")).map_err(|e| e.to_string())?;
        c.validate().map_err(|e| e.to_string())?;
        Ok(c)
    }

    #[test]
    fn allowed_peers_are_optional_bounded_and_never_everything() {
        assert!(parse("").unwrap().allowed_peers.is_empty());
        assert_eq!(parse("allowed_peers = [\"10.77.0.2/32\", \"127.0.0.1/32\"]").unwrap().allowed_peers.len(), 2);
        assert!(parse("allowed_peers = [\"0.0.0.0/0\"]").is_err(), "every address is no allow-list");
        let many = (0..17).map(|i| format!("\"10.0.{i}.0/24\"")).collect::<Vec<_>>().join(", ");
        assert!(parse(&format!("allowed_peers = [{many}]")).is_err(), "at most sixteen");
        assert!(parse("allowed_peers = [\"not a network\"]").is_err());
    }

    #[test]
    fn allowed_hosts_are_dns_names_and_at_most_eight() {
        assert_eq!(parse("allowed_hosts = [\"edge\", \"tasks.edge\"]").unwrap().allowed_hosts.len(), 2);
        assert!(parse("allowed_hosts = [\"a b\"]").is_err());
        let nine = (0..9).map(|i| format!("\"h{i}\"")).collect::<Vec<_>>().join(", ");
        assert!(parse(&format!("allowed_hosts = [{nine}]")).is_err(), "at most eight");
    }

    #[test]
    fn origins_are_scheme_host_and_optional_port_only() {
        for ok in ["https://admin.example.org", "https://admin.example.org:8443", "https://localhost", "https://a-b.c-d.example", "https://10.0.0.1:9443"] {
            assert!(valid_origin(ok), "{ok}");
        }
        for bad in [
            "", "http://admin.example.org", "admin.example.org", "https://", "https://Admin.Example.org", "https://admin.example.org/", "https://admin.example.org/x",
            "https://admin.example.org:", "https://admin.example.org:0", "https://admin.example.org:08443", "https://admin.example.org:99999",
            "https://admin.example.org:443/", "https://user@admin.example.org", "https://admin.example.org?x", "https://admin..example.org", "https://-a.example.org",
            "https://a_b.example.org", "https://admin.example.org:80:90", "https:// admin.example.org", "https://admin.example.org#x", "https://[::1]:8443",
        ] {
            assert!(!valid_origin(bad), "{bad}");
        }
    }
}
