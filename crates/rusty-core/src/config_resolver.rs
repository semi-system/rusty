//! `[resolver]`: the DNS resolver the inbound SMTP service uses for its message checks (SPF, DKIM, DMARC on the mail it receives).
//!
//! Absent: no inbound checks (the service has no DNS of its own: in the standalone stack it sits on private networks with no route out). Present: the checks run, against `nameserver`
//! (the validating resolver of the deployment, docs/design-filters-and-resolver.md) or, without it, the system resolver. The delivery worker has its own `[outbound] nameserver`.

use std::net::SocketAddr;

use serde::Deserialize;

/// `[resolver]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverConfig {
    /// `host:port` of the resolver. Absent: the system resolver.
    #[serde(default)]
    pub nameserver: Option<SocketAddr>,
}

#[cfg(test)]
mod tests {
    use super::ResolverConfig;

    #[test]
    fn the_section_is_a_nameserver_or_nothing() {
        let c: ResolverConfig = toml::from_str("nameserver = \"10.78.0.3:53\"").unwrap();
        assert_eq!(c.nameserver, Some("10.78.0.3:53".parse().unwrap()));
        assert_eq!(toml::from_str::<ResolverConfig>("").unwrap(), ResolverConfig::default());
        assert!(toml::from_str::<ResolverConfig>("nameserver = \"not an address\"").is_err());
        assert!(toml::from_str::<ResolverConfig>("servers = [\"1.1.1.1:53\"]").is_err(), "unknown keys are refused");
    }
}
