//! MX resolution as a Rusty-owned seam (`MxResolver`). The production implementation sits behind the
//! hickory adapter (one DNS stack, shared with SPF/DKIM/DMARC verification); tests use
//! [`StaticDns`], which also pre-seeds the verifier with TXT records.
//!
//! The pure functions here decide the delivery order from an MX answer: preference order with a
//! per-preference rotation seed, the implicit MX of RFC 5321 section 5.1 (no MX records: the domain's
//! own address records), and the null MX of RFC 7505 (a single `.` target: the domain accepts no
//! mail, a permanent failure).

use std::collections::BTreeMap;
use std::net::IpAddr;

use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MxHost {
    pub preference: u16,
    /// Lower-case, no trailing dot.
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MxAnswer {
    Hosts(Vec<MxHost>),
    /// RFC 7505: the domain explicitly accepts no mail.
    NullMx,
    /// The domain exists but has no MX records (use the implicit MX).
    NoRecords,
    /// The domain does not exist.
    NoSuchDomain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsError {
    /// Timeouts, server failures: retry later.
    Temporary(String),
}

#[async_trait]
pub trait MxResolver: Send + Sync {
    async fn mx(&self, domain: &str) -> Result<MxAnswer, DnsError>;
    /// A and AAAA of `host`; empty when the name has no address (or does not exist).
    async fn addresses(&self, host: &str) -> Result<Vec<IpAddr>, DnsError>;
}

/// A host to connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub preference: u16,
}

/// What the destination domain's DNS says, after the RFC rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Try these in order.
    Try(Vec<Target>),
    /// Permanent failure with this reason (null MX, unknown domain).
    Permanent(String),
}

/// Most exchangers tried for one message in one attempt.
pub const MAX_TARGETS: usize = 8;

/// Order an MX answer. `seed` rotates the order inside one preference level so the load spreads
/// over equal-preference hosts; the same seed always gives the same order (testable).
pub fn plan_for(domain: &str, answer: MxAnswer, seed: u64) -> Plan {
    match answer {
        MxAnswer::NullMx => Plan::Permanent(format!("{domain} does not accept mail (null MX)")),
        MxAnswer::NoSuchDomain => Plan::Permanent(format!("domain {domain} does not exist")),
        MxAnswer::NoRecords => Plan::Try(vec![Target { host: domain.to_owned(), preference: 0 }]),
        MxAnswer::Hosts(mut hosts) => {
            hosts.retain(|h| !h.host.is_empty() && h.host != ".");
            if hosts.is_empty() {
                return Plan::Permanent(format!("{domain} has no usable mail exchanger"));
            }
            let mut by_pref: BTreeMap<u16, Vec<String>> = BTreeMap::new();
            for h in hosts {
                by_pref.entry(h.preference).or_default().push(h.host);
            }
            let mut out = Vec::new();
            for (preference, mut group) in by_pref {
                group.sort();
                group.dedup();
                let n = group.len();
                if n > 1 {
                    group.rotate_left(usize::try_from(seed % n as u64).unwrap_or(0));
                }
                out.extend(group.into_iter().map(|host| Target { host, preference }));
            }
            out.truncate(MAX_TARGETS);
            Plan::Try(out)
        }
    }
}

/// A resolver with fixed answers, for tests and for pre-seeding the verifier.
#[derive(Debug, Clone, Default)]
pub struct StaticDns {
    pub mx: BTreeMap<String, MxAnswer>,
    pub addrs: BTreeMap<String, Vec<IpAddr>>,
    /// Names whose lookup fails temporarily.
    pub temp_fail: Vec<String>,
    /// TXT records by name without trailing dot (`sel._domainkey.example.org`), strings joined.
    pub txt: BTreeMap<String, String>,
}

#[async_trait]
impl MxResolver for StaticDns {
    async fn mx(&self, domain: &str) -> Result<MxAnswer, DnsError> {
        if self.temp_fail.iter().any(|d| d == domain) {
            return Err(DnsError::Temporary("simulated DNS failure".into()));
        }
        Ok(self.mx.get(domain).cloned().unwrap_or(MxAnswer::NoSuchDomain))
    }

    async fn addresses(&self, host: &str) -> Result<Vec<IpAddr>, DnsError> {
        if self.temp_fail.iter().any(|d| d == host) {
            return Err(DnsError::Temporary("simulated DNS failure".into()));
        }
        Ok(self.addrs.get(host).cloned().unwrap_or_default())
    }
}

/// The ASCII (A-label) form of a domain, lower-case; `None` when it is not a valid domain name.
pub fn to_ascii_domain(domain: &str) -> Option<String> {
    let d = domain.trim_end_matches('.');
    if d.is_empty() || d.len() > 253 {
        return None;
    }
    let ascii = idna::domain_to_ascii(d).ok()?;
    (!ascii.is_empty() && ascii.len() <= 253).then_some(ascii)
}
