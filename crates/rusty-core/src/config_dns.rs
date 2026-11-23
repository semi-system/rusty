//! `[dns]`: the authoritative name server (docs/design-authoritative-dns.md). Only the `dns` role has this section. It says where to listen, which name servers and which contact the zones carry, and the facts about
//! this installation that the generated records need (the mail host and the addresses mail leaves from: the same facts `[admin]` has, because the zones and the DNS page of the admin web must agree).

use std::net::{IpAddr, SocketAddr};

use serde::Deserialize;

use crate::config::valid_hostname;
use crate::dns::publishable_address;
use crate::{Error, Result};

fn d_listen() -> Vec<SocketAddr> {
    vec![SocketAddr::from(([0, 0, 0, 0], 5353))]
}
fn d_ttl() -> u32 {
    3600
}
fn d_reload() -> u64 {
    15
}
fn d_stale() -> u64 {
    86_400
}
fn d_rate() -> u32 {
    20
}
fn d_burst() -> u32 {
    40
}
fn d_slip() -> u32 {
    2
}
fn d_tcp_max() -> usize {
    1000
}
fn d_tcp_per_ip() -> usize {
    20
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsConfig {
    /// Where to listen, UDP and TCP on each (the container's port; the stack publishes port 53 to it).
    #[serde(default = "d_listen")]
    pub listen: Vec<SocketAddr>,
    /// The names of the name servers, put in the `NS` records of every zone and as the primary of the SOA. At least one; the delegation needs two.
    pub name_servers: Vec<String>,
    /// The person responsible, in the SOA form (`hostmaster.example.org` stands for `hostmaster@example.org`).
    pub hostmaster: String,
    /// The mail host the MX records point at.
    pub mail_host: String,
    /// The addresses mail leaves from (the mail host resolves to them).
    #[serde(default)]
    pub public_ips: Vec<IpAddr>,
    /// The time to live of the records the platform generates.
    #[serde(default = "d_ttl")]
    pub ttl: u32,
    /// How often the zones are read again from the database, in seconds.
    #[serde(default = "d_reload")]
    pub reload_secs: u64,
    /// How long the last good zones are served when the database cannot be read; after that the zones answer SERVFAIL.
    #[serde(default = "d_stale")]
    pub max_stale_secs: u64,
    /// What one source prefix (a /24, a /56) may ask each second, and in a burst; one in `slip` questions over that gets a truncated answer.
    #[serde(default = "d_rate")]
    pub rate_per_second: u32,
    #[serde(default = "d_burst")]
    pub burst: u32,
    #[serde(default = "d_slip")]
    pub slip: u32,
    /// TCP connections at once and from one address.
    #[serde(default = "d_tcp_max")]
    pub tcp_max: usize,
    #[serde(default = "d_tcp_per_ip")]
    pub tcp_per_ip: usize,
}

impl DnsConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        let bad = |m: &str| Err(Error::Invalid(format!("dns.{m}")));
        if self.listen.is_empty() || self.listen.len() > 8 {
            return bad("listen must be one to eight addresses");
        }
        if self.name_servers.is_empty() || self.name_servers.len() > 8 || !self.name_servers.iter().all(|n| valid_hostname(n) && !n.ends_with('.')) {
            return bad("name_servers must be one to eight host names without a trailing dot");
        }
        if !valid_hostname(&self.hostmaster) || self.hostmaster.ends_with('.') {
            return bad("hostmaster must be a host name without a trailing dot (hostmaster.example.org)");
        }
        if !valid_hostname(&self.mail_host) || self.mail_host.ends_with('.') {
            return bad("mail_host must be a host name without a trailing dot");
        }
        if self.public_ips.len() > 16 || !self.public_ips.iter().all(|ip| publishable_address(*ip)) {
            return bad("public_ips must be at most 16 addresses that can be published");
        }
        if !(60..=86_400).contains(&self.ttl) {
            return bad("ttl must be 60..=86400 seconds");
        }
        if !(1..=3600).contains(&self.reload_secs) || !(60..=2_592_000).contains(&self.max_stale_secs) {
            return bad("reload_secs must be 1..=3600 and max_stale_secs 60..=2592000");
        }
        if !(1..=100_000).contains(&self.rate_per_second) || !(1..=1_000_000).contains(&self.burst) || self.slip > 100 {
            return bad("rate_per_second, burst or slip is out of range");
        }
        if !(1..=100_000).contains(&self.tcp_max) || !(1..=10_000).contains(&self.tcp_per_ip) {
            return bad("tcp_max or tcp_per_ip is out of range");
        }
        Ok(())
    }
}
