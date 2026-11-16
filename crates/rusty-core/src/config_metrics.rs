//! `[metrics]`: the listener that serves `GET /metrics` in the Prometheus text format (docs/design-metrics.md). Any service may have it, on a listener of its own, never on a mail or admin port.
//!
//! Absent: no listener. The endpoint has no login, so `allowed_peers` is required: one to sixteen address blocks (the scraper), none that holds every address. Any other peer is dropped before a byte is read.

use std::net::SocketAddr;

use serde::Deserialize;

use crate::cidr::Cidr;
use crate::{Error, Result};

/// `[metrics]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// Where the endpoint listens (`0.0.0.0:9100` in a container).
    pub listen: SocketAddr,
    /// The networks that may scrape it.
    pub allowed_peers: Vec<Cidr>,
}

impl MetricsConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.allowed_peers.is_empty() || self.allowed_peers.len() > 16 || self.allowed_peers.iter().any(Cidr::is_everything) {
            return Err(Error::Invalid("metrics.allowed_peers must name one to sixteen address blocks and none may hold every address".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MetricsConfig;

    fn parse(text: &str) -> Result<MetricsConfig, String> {
        let c: MetricsConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        c.validate().map_err(|e| e.to_string())?;
        Ok(c)
    }

    #[test]
    fn the_section_names_where_it_listens_and_who_may_scrape() {
        let c = parse("listen = \"0.0.0.0:9100\"\nallowed_peers = [\"10.79.0.2/32\", \"127.0.0.1/32\"]").unwrap();
        assert_eq!((c.listen.port(), c.allowed_peers.len()), (9100, 2));
        assert!(parse("listen = \"0.0.0.0:9100\"").is_err(), "no scraper named");
        assert!(parse("listen = \"0.0.0.0:9100\"\nallowed_peers = []").is_err(), "an empty list is not a list");
        assert!(parse("listen = \"0.0.0.0:9100\"\nallowed_peers = [\"0.0.0.0/0\"]").is_err(), "every address is no restriction");
        assert!(parse("listen = \"0.0.0.0:9100\"\nallowed_peers = [\"::/0\"]").is_err());
        let many = (0..17).map(|i| format!("\"10.0.{i}.0/24\"")).collect::<Vec<_>>().join(", ");
        assert!(parse(&format!("listen = \"0.0.0.0:9100\"\nallowed_peers = [{many}]")).is_err(), "at most sixteen");
        assert!(parse("listen = \"nonsense\"\nallowed_peers = [\"10.0.0.1/32\"]").is_err());
        assert!(parse("listen = \"0.0.0.0:9100\"\nallowed_peers = [\"10.0.0.1/32\"]\nextra = 1").is_err(), "unknown keys are refused");
    }
}
