//! `ServiceEvents`: the metrics and audit seam. Services report what happened (connection,
//! authentication result, rejection); the default implementation writes `tracing` events. A
//! metrics or audit-log implementation replaces it without touching a service. Events carry no
//! credentials and no message content.

use std::net::SocketAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    TotalLimit,
    PerIpLimit,
    HandshakeFailed,
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub enum ServiceEvent<'a> {
    Accepted { service: &'a str, peer: SocketAddr },
    Rejected { service: &'a str, peer: SocketAddr, reason: RejectReason },
    Closed { service: &'a str, peer: SocketAddr },
    SessionPanicked { service: &'a str },
    AuthResult { service: &'a str, peer: SocketAddr, ok: bool },
}

pub trait ServiceEvents: Send + Sync {
    fn emit(&self, event: ServiceEvent<'_>);
}

/// The default: one `tracing` event per service event.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingEvents;

impl ServiceEvents for TracingEvents {
    fn emit(&self, event: ServiceEvent<'_>) {
        match event {
            ServiceEvent::Accepted { service, peer } => tracing::debug!(service, %peer, "connection accepted"),
            ServiceEvent::Rejected { service, peer, reason } => {
                tracing::warn!(service, %peer, ?reason, "connection rejected")
            }
            ServiceEvent::Closed { service, peer } => tracing::debug!(service, %peer, "connection closed"),
            ServiceEvent::SessionPanicked { service } => tracing::error!(service, "session task panicked"),
            ServiceEvent::AuthResult { service, peer, ok } => {
                tracing::info!(service, %peer, ok, "authentication attempt")
            }
        }
    }
}
