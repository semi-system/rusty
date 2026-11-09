//! Shared plumbing of the Rusty services (U1): TLS, the listener loop, graceful shutdown,
//! logging, the `Context` every protocol crate is built from, and the seam traits of the plan
//! (`Accept`, `SecretSource`, `CertResolver`, `RateLimiter`, `QuotaPolicy`, `ServiceEvents`,
//! `OutboundEnqueue`).
//!
//! Protocol crates (`rusty-smtp`, `rusty-imap`) depend on this crate and talk to storage only
//! through the facades held by [`Context`]. Nothing here is global: every service is built from
//! `(config, trait objects)`.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

mod accept;
mod budget;
mod context;
mod events;
mod gc;
mod hub;
mod listener;
mod logging;
mod outbound;
mod plan;
mod quota;
mod redact;
mod secret;
mod service;
mod throttle;
mod tls;

pub use budget::MemoryBudget;
pub use accept::{Accept, AsyncStream, BoxedStream, DirectTcp, DirectTls, PeerInfo, DEFAULT_HANDSHAKE_TIMEOUT};
pub use context::Context;
pub use events::{RejectReason, ServiceEvent, ServiceEvents, TracingEvents};
pub use gc::{GcService, GcSummary};
pub use hub::MailboxHub;
pub use listener::{BoundListener, ListenerSpec, SessionHandler};
pub use logging::{init_logging, subscriber};
pub use outbound::{OutboundEnqueue, OutboundMessage};
pub use plan::{listener_plan, ListenerKind, PlannedListener};
pub use quota::{QuotaPolicy, SizeCap};
pub use redact::{redact_command, Redacted};
pub use secret::{FileSecretSource, SecretSource, MAX_SECRET_BYTES};
pub use service::{run_services, ListenerService, Service};
pub use throttle::{AttemptPermit, AuthThrottle, Clock, RateLimiter, SystemClock, Verdict};
pub use tls::{
    certified_key, parse_cert_chain, parse_private_key, CertResolver, FileCertResolver, StaticCertResolver,
    TlsProvider, MAX_CERT_CHAIN,
};
