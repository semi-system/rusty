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
    use super::valid_origin;

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
