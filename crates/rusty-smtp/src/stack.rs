//! The DNS-and-authentication stack as one Rusty-owned value: an [`MxResolver`] for the outbound
//! worker and an [`InboundAuthChecker`] for port 25, both on one resolver. The implementations are
//! behind the adapters; this is the composition root's entry point.

use std::sync::Arc;

use std::net::IpAddr;

use rusty_core::config::OutboundConfig;
use rusty_core::dns_check::{DnsFail, DnsSource, MxEntry, SrvEntry};
use rusty_core::Result;

use crate::adapter;
use crate::authres::InboundAuthChecker;
use crate::resolve::{MxResolver, StaticDns};

#[derive(Clone)]
pub struct DnsStack {
    pub resolver: Arc<dyn MxResolver>,
    pub checker: Arc<dyn InboundAuthChecker>,
}

impl DnsStack {
    /// The live stack: the system resolver, or `[outbound] nameserver` when set.
    pub fn from_config(cfg: &OutboundConfig, hostname: &str) -> Result<DnsStack> {
        let (resolver, checker) = adapter::dns::live_stack(cfg.nameserver, hostname)?;
        Ok(DnsStack { resolver, checker })
    }

    /// Answers from fixed records, never from the network: MX and address lookups from `dns`, and
    /// SPF/DKIM/DMARC records for the inbound checker. For tests of the worker and of verification.
    pub fn with_static(dns: StaticDns, hostname: &str) -> Result<DnsStack> {
        let checker = adapter::dns::static_checker(&dns, hostname)?;
        Ok(DnsStack { resolver: Arc::new(dns), checker })
    }
}

/// The live DNS source of the validation engine (`rusty_core::dns_check::DnsSource`): the resolver of
/// `[outbound] nameserver`, or the system resolver. Used by `rustyctl` and by the `dns.check` job.
pub struct LiveDns(adapter::dns::LiveSource);

impl LiveDns {
    pub fn from_config(cfg: &OutboundConfig) -> Result<LiveDns> {
        Ok(LiveDns(adapter::dns::LiveSource::new(cfg.nameserver)?))
    }
}

impl DnsSource for LiveDns {
    async fn txt(&self, name: &str) -> std::result::Result<Vec<String>, DnsFail> {
        self.0.txt(name).await
    }
    async fn mx(&self, name: &str) -> std::result::Result<Vec<MxEntry>, DnsFail> {
        self.0.mx(name).await
    }
    async fn addresses(&self, name: &str) -> std::result::Result<Vec<IpAddr>, DnsFail> {
        self.0.addresses(name).await
    }
    async fn ptr(&self, ip: IpAddr) -> std::result::Result<Vec<String>, DnsFail> {
        self.0.ptr(ip).await
    }
    async fn srv(&self, name: &str) -> std::result::Result<Vec<SrvEntry>, DnsFail> {
        self.0.srv(name).await
    }
}
