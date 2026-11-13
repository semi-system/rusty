//! The DNS-and-authentication stack as one Rusty-owned value: an [`MxResolver`] for the outbound
//! worker and an [`InboundAuthChecker`] for port 25, both on one resolver. The implementations are
//! behind the adapters; this is the composition root's entry point.

use std::sync::Arc;

use rusty_core::config::OutboundConfig;
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
