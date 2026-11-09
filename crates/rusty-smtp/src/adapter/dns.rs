//! The hickory resolver (reached through `mail-auth`, which wraps it and re-exports it, so there is
//! exactly one DNS stack and one version of it) behind the Rusty-owned `MxResolver`, plus the
//! pre-seeded record cache used instead of DNS in tests. Nothing of hickory or `mail-auth` leaves
//! this file through a public item.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mail_auth::dkim::DomainKey;
use mail_auth::dmarc::DmarcRecord;
use mail_auth::hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig};
use mail_auth::hickory_resolver::proto::op::ResponseCode;
use mail_auth::spf::SpfRecord;
use mail_auth::{
    DnsCache, DnsError as LibDnsError, Error as LibError, MessageAuthenticator, Mx, Negative, RecordSet, ResolverCache, TxtRecord,
};
use mail_auth::dns::TxtRecordParser;
use mail_auth::dmarc::verify::DmarcParameters;
use mail_auth::spf::verify::SpfParameters;
use mail_auth::{AuthenticatedMessage, AuthenticationResults, DkimResult, DmarcResult, Parameters, SpfResult};
use crate::authres::{AuthInput, AuthResults, InboundAuthChecker};

use crate::resolve::{DnsError, MxAnswer, MxHost, MxResolver, StaticDns};

/// The verifier/resolver stack: system resolver configuration, or one explicit nameserver
/// (`[outbound] nameserver`, an operator setting read from the configuration file only).
fn authenticator(nameserver: Option<SocketAddr>) -> rusty_core::Result<MessageAuthenticator> {
    let built = match nameserver {
        None => MessageAuthenticator::new_system_conf(),
        Some(addr) => {
            let mut udp = ConnectionConfig::udp();
            let mut tcp = ConnectionConfig::tcp();
            udp.port = addr.port();
            tcp.port = addr.port();
            let ns = NameServerConfig::new(addr.ip(), true, vec![udp, tcp]);
            let mut opts = mail_auth::hickory_resolver::config::ResolverOpts::default();
            opts.attempts = 2;
            opts.timeout = Duration::from_secs(5);
            MessageAuthenticator::new(ResolverConfig::from_parts(None, vec![], vec![ns]), opts)
        }
    };
    built.map_err(|_| rusty_core::Error::Invalid("cannot set up the DNS resolver".into()))
}

struct LibMx {
    authenticator: MessageAuthenticator,
}

fn not_found(e: &LibError) -> bool {
    matches!(e, LibError::Dns(LibDnsError::RecordNotFound(_)))
}

#[async_trait]
impl MxResolver for LibMx {
    async fn mx(&self, domain: &str) -> Result<MxAnswer, DnsError> {
        let cache: Option<&mail_auth::NoCache> = None;
        match self.authenticator.mx_lookup(domain, cache).await {
            Ok(set) => {
                let mut hosts = Vec::new();
                for mx in set.records.iter() {
                    for ex in mx.exchanges.iter() {
                        let host = ex.trim_end_matches('.').to_ascii_lowercase();
                        hosts.push(MxHost { preference: mx.preference, host });
                    }
                }
                if hosts.len() == 1 && hosts.first().is_some_and(|h| h.host.is_empty()) {
                    return Ok(MxAnswer::NullMx);
                }
                if hosts.is_empty() {
                    return Ok(MxAnswer::NoRecords);
                }
                Ok(MxAnswer::Hosts(hosts))
            }
            Err(e) if not_found(&e) => {
                // NXDOMAIN versus an empty answer: ask for any address to tell them apart.
                match self.addresses(domain).await? {
                    a if a.is_empty() => Ok(MxAnswer::NoSuchDomain),
                    _ => Ok(MxAnswer::NoRecords),
                }
            }
            Err(_) => Err(DnsError::Temporary("MX lookup failed".into())),
        }
    }

    async fn addresses(&self, host: &str) -> Result<Vec<IpAddr>, DnsError> {
        let c4: Option<&mail_auth::NoCache> = None;
        let c6: Option<&mail_auth::NoCache> = None;
        match self
            .authenticator
            .ip_lookup(host, mail_auth::IpLookupStrategy::Ipv4thenIpv6, 16, c4, c6)
            .await
        {
            Ok(a) => Ok(a),
            Err(e) if not_found(&e) => Ok(Vec::new()),
            Err(_) => Err(DnsError::Temporary("address lookup failed".into())),
        }
    }
}

// ------------------------------------------------------------------ pre-seeded records

struct Map<K, V> {
    entries: Mutex<HashMap<K, V>>,
    /// The answer for a name that was not seeded: authoritative non-existence, so a verifier walking
    /// up the DNS tree gets an answer instead of a resolver failure.
    missing: fn() -> V,
}

impl<K: std::hash::Hash + Eq, V: Clone> ResolverCache<K, V> for Map<K, V> {
    fn get<Q>(&self, name: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        {
            let found = self.entries.lock().ok()?.get(name).cloned();
            Some(found.unwrap_or_else(self.missing))
        }
    }
    fn remove<Q>(&self, name: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.entries.lock().ok()?.remove(name)
    }
    fn insert(&self, key: K, value: V, _valid_until: Instant) {
        if let Ok(mut m) = self.entries.lock() {
            m.insert(key, value);
        }
    }
}

type Answer<T> = Result<RecordSet<T>, Negative>;

fn nx<T>() -> Answer<T> {
    Err(Negative { code: ResponseCode::NXDomain })
}

/// Records the verifier finds instead of querying DNS.
struct StaticCache {
    txt: Map<Box<str>, TxtRecord>,
    mx: Map<Box<str>, Answer<Mx>>,
    ipv4: Map<Box<str>, Answer<Ipv4Addr>>,
    ipv6: Map<Box<str>, Answer<Ipv6Addr>>,
    ptr: Map<IpAddr, Answer<Box<str>>>,
}

fn fqdn(name: &str) -> Box<str> {
    format!("{}.", name.trim_end_matches('.').to_ascii_lowercase()).into_boxed_str()
}

impl StaticCache {
    pub(crate) fn from_static(dns: &StaticDns) -> StaticCache {
        let c = StaticCache {
            txt: Map { entries: Default::default(), missing: || TxtRecord::Error(LibError::Dns(LibDnsError::RecordNotFound(ResponseCode::NXDomain))) },
            mx: Map { entries: Default::default(), missing: nx },
            ipv4: Map { entries: Default::default(), missing: nx },
            ipv6: Map { entries: Default::default(), missing: nx },
            ptr: Map { entries: Default::default(), missing: nx },
        };
        let until = Instant::now() + Duration::from_secs(3600);
        for (name, value) in &dns.txt {
            let bytes = value.as_bytes();
            let record: Option<TxtRecord> = if name.contains("._domainkey.") {
                DomainKey::parse(bytes).ok().map(Into::into)
            } else if name.starts_with("_dmarc.") {
                DmarcRecord::parse(bytes).ok().map(Into::into)
            } else {
                SpfRecord::parse(bytes).ok().map(Into::into)
            };
            if let Some(r) = record {
                c.txt.insert(fqdn(name), r, until);
            }
        }
        for (host, addrs) in &dns.addrs {
            let v4: Vec<Ipv4Addr> = addrs.iter().filter_map(|a| if let IpAddr::V4(a) = a { Some(*a) } else { None }).collect();
            let v6: Vec<Ipv6Addr> = addrs.iter().filter_map(|a| if let IpAddr::V6(a) = a { Some(*a) } else { None }).collect();
            if !v4.is_empty() {
                let records: Arc<[Ipv4Addr]> = Arc::from(v4.into_boxed_slice());
                c.ipv4.insert(fqdn(host), Ok(RecordSet { records, dnssec_status: mail_auth::DnssecStatus::Indeterminate }), until);
            }
            if !v6.is_empty() {
                let records: Arc<[Ipv6Addr]> = Arc::from(v6.into_boxed_slice());
                c.ipv6.insert(fqdn(host), Ok(RecordSet { records, dnssec_status: mail_auth::DnssecStatus::Indeterminate }), until);
            }
        }
        c
    }
}

impl DnsCache for StaticCache {
    type Txt = Map<Box<str>, TxtRecord>;
    type Mx = Map<Box<str>, Answer<Mx>>;
    type Ipv4 = Map<Box<str>, Answer<Ipv4Addr>>;
    type Ipv6 = Map<Box<str>, Answer<Ipv6Addr>>;
    type Ptr = Map<IpAddr, Answer<Box<str>>>;

    fn txt(&self) -> Option<&Self::Txt> {
        Some(&self.txt)
    }
    fn mx(&self) -> Option<&Self::Mx> {
        Some(&self.mx)
    }
    fn ipv4(&self) -> Option<&Self::Ipv4> {
        Some(&self.ipv4)
    }
    fn ipv6(&self) -> Option<&Self::Ipv6> {
        Some(&self.ipv6)
    }
    fn ptr(&self) -> Option<&Self::Ptr> {
        Some(&self.ptr)
    }
}
// ------------------------------------------------------------------ verification

/// Verifies the SPF identity, DKIM signatures and DMARC alignment of an inbound message and renders
/// the `Authentication-Results` value. Log-only: it never decides about the message.
struct LibChecker {
    authenticator: MessageAuthenticator,
    hostname: String,
    /// Pre-seeded records instead of live DNS (tests).
    cache: Option<Arc<StaticCache>>,
}

fn spf_name(r: SpfResult) -> &'static str {
    match r {
        SpfResult::Pass => "pass",
        SpfResult::Fail => "fail",
        SpfResult::SoftFail => "softfail",
        SpfResult::Neutral => "neutral",
        SpfResult::TempError => "temperror",
        SpfResult::PermError => "permerror",
        SpfResult::None => "none",
    }
}

fn dkim_name(r: &DkimResult) -> &'static str {
    match r {
        DkimResult::Pass => "pass",
        DkimResult::Neutral(_) => "neutral",
        DkimResult::Fail(_) => "fail",
        DkimResult::PermError(_) => "permerror",
        DkimResult::TempError(_) => "temperror",
        DkimResult::None => "none",
    }
}

fn dmarc_name(r: &DmarcResult) -> &'static str {
    match r {
        DmarcResult::Pass => "pass",
        DmarcResult::Fail(_) => "fail",
        DmarcResult::PermError(_) => "permerror",
        DmarcResult::TempError(_) => "temperror",
        DmarcResult::None => "none",
    }
}

#[async_trait::async_trait]
impl InboundAuthChecker for LibChecker {
    async fn check(&self, input: &AuthInput<'_>) -> AuthResults {
        let Some(message) = AuthenticatedMessage::parse(input.message) else {
            return AuthResults::none(&self.hostname);
        };
        let helo = if input.helo.is_empty() { "unknown" } else { input.helo };
        let mail_from = input.mail_from;
        let mf_domain = mail_from.rsplit_once('@').map_or(helo, |(_, d)| d);
        let spf_params = SpfParameters::helo_and_mail_from(input.peer, helo, &self.hostname, mail_from);
        // The two branches differ only in the cache type of the library's `Parameters`.
        macro_rules! run {
            ($wrap:expr) => {{
                let spf = self.authenticator.verify_spf($wrap(spf_params)).await;
                let dkim = self.authenticator.verify_dkim($wrap(&message)).await;
                let dmarc = self
                    .authenticator
                    .verify_dmarc($wrap(DmarcParameters::new(&message, &dkim, mf_domain, &spf)))
                    .await;
                let header = AuthenticationResults::new(&self.hostname)
                    .with_dkim_results(&dkim, message.first_from_address())
                    .with_spf_result(&spf, &spf_params)
                    .with_dmarc_result(&dmarc)
                    .to_string()
                    .replace(['\r', '\n'], " ");
                AuthResults {
                    header_value: header,
                    spf: spf_name(spf.result()).to_owned(),
                    dkim: dkim.first().map_or("none", |d| dkim_name(d.result())).to_owned(),
                    dmarc: dmarc_name(&dmarc.result()).to_owned(),
                }
            }};
        }
        match &self.cache {
            Some(c) => run!(|p| Parameters::new(p).with_cache(c.as_ref())),
            None => run!(Parameters::new),
        }
    }
}

/// Resolver and inbound checker built on the same DNS stack.
pub(crate) fn live_stack(
    nameserver: Option<SocketAddr>,
    hostname: &str,
) -> rusty_core::Result<(Arc<dyn MxResolver>, Arc<dyn InboundAuthChecker>)> {
    let authenticator = authenticator(nameserver)?;
    let resolver: Arc<dyn MxResolver> = Arc::new(LibMx { authenticator: authenticator.clone() });
    let checker: Arc<dyn InboundAuthChecker> = Arc::new(LibChecker { authenticator, hostname: hostname.to_owned(), cache: None });
    Ok((resolver, checker))
}

/// An inbound checker that answers from `dns` instead of the network (tests). Every name the
/// verifier needs is in the cache (or answered as non-existent); nothing is ever queried.
pub(crate) fn static_checker(dns: &StaticDns, hostname: &str) -> rusty_core::Result<Arc<dyn InboundAuthChecker>> {
    let dead: SocketAddr = "127.0.0.1:9".parse().map_err(|_| rusty_core::Error::Invalid("address".into()))?;
    Ok(Arc::new(LibChecker {
        authenticator: authenticator(Some(dead))?,
        hostname: hostname.to_owned(),
        cache: Some(Arc::new(StaticCache::from_static(dns))),
    }))
}
