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
    /// The services this process reads and serves in its own exposition (the admin web only: the one place to scrape). Empty: none.
    #[serde(default)]
    pub scrape: Vec<ScrapeTarget>,
}

/// `[[metrics.scrape]]`: a service to read, by name and by `host:port` of its metrics listener (the host may resolve to many addresses: each is an instance).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrapeTarget {
    pub service: String,
    pub address: String,
}

fn name_ok(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn address_ok(s: &str) -> bool {
    let Some((host, port)) = s.rsplit_once(':') else {
        return false;
    };
    (1..=253).contains(&host.len())
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        && port.parse::<u16>().is_ok_and(|p| p != 0)
}

impl MetricsConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.allowed_peers.is_empty() || self.allowed_peers.len() > 16 || self.allowed_peers.iter().any(Cidr::is_everything) {
            return Err(Error::Invalid("metrics.allowed_peers must name one to sixteen address blocks and none may hold every address".into()));
        }
        if self.scrape.len() > 32 {
            return Err(Error::Invalid(
                "metrics.scrape holds at most 32 targets".into(),
            ));
        }
        for t in &self.scrape {
            if !name_ok(&t.service) || !address_ok(&t.address) {
                return Err(Error::Invalid("a metrics.scrape target needs a service name (a-z, 0-9, - and _) and an address host:port".into()));
            }
        }
        if self
            .scrape
            .iter()
            .enumerate()
            .any(|(i, t)| self.scrape[..i].iter().any(|u| u.service == t.service))
        {
            return Err(Error::Invalid(
                "metrics.scrape names a service twice".into(),
            ));
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
