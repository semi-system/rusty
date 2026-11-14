//! `check_relay`: the operator's answer to "can this server send through the configured relay?".
//!
//! It runs the steps the worker runs before it sends (resolve the relay's name, connect, TLS exactly as
//! the worker's policy says, the login) through the same code (`client::probe_session`), and stops
//! there: no MAIL FROM, no message, nothing queued. A wrong password, an untrusted certificate or a
//! relay that offers no login the client speaks therefore shows up now, not as mail that waits in the
//! queue for days.
//!
//! Failures are one fixed category each, with a fixed text: no password, user name, host, address or
//! text of the remote server ever appears in an error (the report of a success does name the host and
//! the address, which the operator configured).

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use rusty_core::config::{OutboundConfig, RelayTls, TlsPolicy};
use rusty_svc::SecretSource;

use super::client::{probe_session, HostFailure, SessionParams, Timeouts};
use super::dial::Dialer;
use super::route::{ConfigRoute, OutboundRoute, Route};
use super::tls::{TlsInfo, TlsUpgrader};
use crate::resolve::MxResolver;

/// Addresses of the relay tried at most (like the worker).
const MAX_ADDRESSES: usize = 4;

/// What the check needs from the outside, all injected (the production values are the ones the worker
/// gets; the tests use loopback doubles).
pub struct RelayCheckDeps<'a> {
    pub resolver: &'a dyn MxResolver,
    pub dialer: &'a dyn Dialer,
    pub tls: &'a dyn TlsUpgrader,
    pub secrets: &'a dyn SecretSource,
    /// The name given in EHLO.
    pub helo: &'a str,
}

/// A relay that works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayReport {
    pub host: String,
    pub address: IpAddr,
    pub port: u16,
    pub tls: Option<TlsInfo>,
    /// The login mechanisms the relay offered after TLS (sanitised).
    pub offered: Vec<String>,
    /// The mechanism that was used; `None` when no credentials are configured.
    pub mechanism: Option<String>,
    pub size_limit: Option<u64>,
}

/// Why a relay cannot be used. The text of every variant is fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayFailure {
    /// `[outbound.relay]` is not set: mail goes straight to the recipients' servers.
    NotConfigured,
    /// The password file could not be read (missing, wrong mode, not one line).
    Credentials,
    /// The relay's name does not resolve to an address.
    Dns,
    /// No address of the relay accepted a connection in time.
    Connect,
    /// TLS: not offered, refused, or the certificate does not verify.
    Tls,
    /// The relay refused the login.
    Auth,
    /// The relay offers no login mechanism this server speaks (PLAIN, LOGIN); `offered` is what it does offer.
    NoMechanism { offered: Vec<String> },
    /// The conversation failed (no greeting in time, an invalid or refusing reply).
    Protocol,
}

impl fmt::Display for RelayFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RelayFailure::NotConfigured => "no relay is configured ([outbound.relay])",
            RelayFailure::Credentials => "the relay password file cannot be used (it must exist, be mode 0600 and hold one line)",
            RelayFailure::Dns => "the relay's name does not resolve",
            RelayFailure::Connect => "cannot connect to the relay (nothing accepted a connection in time)",
            RelayFailure::Tls => "TLS to the relay failed (not offered, refused, or the certificate does not verify)",
            RelayFailure::Auth => "the relay refused the user name or password",
            RelayFailure::NoMechanism { .. } => "the relay offers no login mechanism this server speaks (PLAIN or LOGIN)",
            RelayFailure::Protocol => "the relay did not complete the SMTP conversation",
        })
    }
}

impl std::error::Error for RelayFailure {}

pub async fn check_relay(cfg: &OutboundConfig, deps: &RelayCheckDeps<'_>) -> Result<RelayReport, RelayFailure> {
    if cfg.relay.is_none() {
        return Err(RelayFailure::NotConfigured);
    }
    // The same code the worker uses reads the password (a bad file is refused here, before any connection).
    let route = ConfigRoute::from_config(cfg, deps.secrets).map_err(|_| RelayFailure::Credentials)?;
    let Ok(Route::Relay(spec)) = route.route("", "").await else { return Err(RelayFailure::NotConfigured) };

    let addrs = if let Ok(ip) = spec.host.parse::<IpAddr>() {
        vec![ip]
    } else {
        match deps.resolver.addresses(&spec.host).await {
            Ok(a) if !a.is_empty() => a,
            _ => return Err(RelayFailure::Dns),
        }
    };

    let policy = if spec.tls == RelayTls::None { TlsPolicy::None } else { TlsPolicy::Verified };
    let implicit = spec.tls == RelayTls::Implicit;
    let timeouts = Timeouts { command: Duration::from_secs(cfg.command_timeout_secs), data: Duration::from_secs(cfg.data_timeout_secs) };
    let connect = Duration::from_secs(cfg.connect_timeout_secs);
    let auth = spec.credentials.as_ref().map(|c| (c.username.as_str(), c.password.as_str()));

    let mut failure = RelayFailure::Connect;
    for ip in addrs.into_iter().take(MAX_ADDRESSES) {
        let Ok(stream) = deps.dialer.dial(ip, spec.port, None, connect).await else { continue };
        let params = SessionParams { helo: deps.helo, server_name: &spec.host, policy, implicit_tls: implicit, auth, timeouts };
        match probe_session(stream, &params, deps.tls).await {
            Ok(p) => {
                return Ok(RelayReport {
                    host: spec.host.clone(),
                    address: ip,
                    port: spec.port,
                    tls: p.tls,
                    offered: p.offered,
                    mechanism: p.mechanism.map(str::to_owned),
                    size_limit: p.size_limit,
                })
            }
            Err(HostFailure::TlsHandshake(_) | HostFailure::TlsRequired(_)) => failure = RelayFailure::Tls,
            Err(HostFailure::AuthRejected) => failure = RelayFailure::Auth,
            Err(HostFailure::NoAuthMechanism { offered }) => failure = RelayFailure::NoMechanism { offered },
            Err(HostFailure::Temporary(_) | HostFailure::Permanent { .. }) => failure = RelayFailure::Protocol,
        }
    }
    Err(failure)
}
