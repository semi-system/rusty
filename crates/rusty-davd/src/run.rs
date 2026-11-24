//! Start the calendar and contacts service: the pieces assembled from a configuration and an opened store (the `rusty-dav` binary is a thin `main` over this).
//!
//! * The sign-in failures are counted in the shared key-value store when `[valkey]` is configured (several instances share the counters); a configured Valkey that cannot be reached **fails the start**.
//! * TLS terminates at the service when `[tls]` names a certificate, otherwise at the edge in front of it. Behind the edge only: `allowed_peers` and `allowed_hosts` drop every other peer before a byte
//!   is read, and `proxy_protocol` takes the client's address from the edge's header.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rusty_core::config::StartupConfig;
use rusty_core::{Error, Result};
use rusty_dav::server::Dav;
use rusty_directory::{Directory, LocalAuthenticator};
use rusty_http::{HttpConfig, HttpHandler, Limits, Router};
use rusty_store::Store;
use rusty_svc::{Accept, AuthThrottle, BoundListener, DirectTcp, DirectTls, FileSecretSource, ListenerSpec, PeerAllow, SecretSource, TlsProvider, TracingEvents};
use rusty_valkey::MemoryKv;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::auth::DavAuth;
use crate::handler::{routes, DavApp};

/// Connections the listener accepts at once, and from one address (a phone and a laptop behind one address, and an office behind another).
const MAX_CONNECTIONS: u32 = 1024;
const MAX_PER_IP: u32 = 128;
/// The largest request body: an object (1 MiB) and a little over; the objects themselves are checked by the service.
const MAX_BODY_BYTES: usize = 1024 * 1024 + 4096;

pub struct Running {
    /// Where it listens (useful when `listen` asked for port 0).
    pub addr: SocketAddr,
    /// The TLS provider, when the service terminates TLS (for SIGHUP reloads).
    pub tls: Option<TlsProvider>,
    pub task: JoinHandle<Result<()>>,
}

async fn throttle_kv(cfg: &StartupConfig) -> Result<Arc<dyn rusty_valkey::Kv>> {
    let Some(v) = &cfg.valkey else { return Ok(Arc::new(MemoryKv::new())) };
    let password = match &v.password_file {
        Some(p) => Some(String::from_utf8(FileSecretSource::new().read(p)?).map_err(|_| Error::Invalid("valkey.password_file is not text".into()))?.trim_end().to_owned()),
        None => None,
    };
    let tls = if v.tls {
        let ca = match &v.ca_bundle {
            Some(p) => Some(std::fs::read(p).map_err(|_| Error::Invalid("valkey.ca_bundle cannot be read".into()))?),
            None => None,
        };
        Some(rusty_valkey::TlsOptions { ca_bundle_pem: ca })
    } else {
        None
    };
    let kv = rusty_valkey::ValkeyKv::connect(rusty_valkey::ValkeyOptions {
        host: v.host.clone(),
        port: v.port,
        tls,
        username: v.username.clone(),
        password,
        connect_timeout: Duration::from_millis(v.connect_timeout_ms),
        command_timeout: Duration::from_millis(v.command_timeout_ms),
        key_prefix: format!("{}:dav:", v.key_prefix.trim_end_matches(':')),
    })
    .await?;
    tracing::info!(host = %v.host, "the sign-in counters are kept in Valkey");
    Ok(Arc::new(kv))
}

/// Bind and serve. The listener is bound before this returns (a clash fails the start); `shutdown` drains it.
pub async fn start(cfg: &StartupConfig, store: &Store, shutdown: CancellationToken) -> Result<Running> {
    let Some(dav) = cfg.dav.clone() else { return Err(Error::Invalid("the DAV service needs a [dav] section".into())) };
    let repos = store.repos();
    let directory = Directory::with_repo(repos.directory.clone());
    let kv = throttle_kv(cfg).await?;
    let throttle: Arc<dyn rusty_svc::RateLimiter> = match &cfg.valkey {
        Some(_) => Arc::new(rusty_svc::ValkeyThrottle::from_limits(kv, &cfg.limits)),
        None => Arc::new(AuthThrottle::from_limits(&cfg.limits)),
    };
    let authenticator = Arc::new(LocalAuthenticator::new(directory.clone()).await?.with_app_passwords(repos.app_passwords.clone()));
    let auth = DavAuth::new(authenticator, directory, throttle);
    let app = Arc::new(DavApp { dav: Dav::new(repos.dav.clone()), auth });

    // The heartbeat of the service itself, for the Monitor page of the admin web.
    {
        let beat = rusty_svc::HeartbeatService::new(repos.heartbeat.clone(), "dav", env!("CARGO_PKG_VERSION")).with_detail("{\"listeners\":1}".to_owned());
        let stop = shutdown.clone();
        tokio::spawn(async move { beat.run_until(stop).await });
    }
    let router = routes(app, Router::new());
    let http = HttpConfig { limits: Limits { max_body_bytes: MAX_BODY_BYTES, allow_trailing_slash: true, ..Limits::default() } };
    let handler = Arc::new(HttpHandler::new(router, http));

    let (accept, tls): (Arc<dyn Accept>, Option<TlsProvider>) = if cfg.tls.has_cert() {
        let hostname = cfg.hostname.clone().unwrap_or_else(|| "localhost".to_owned());
        let provider = TlsProvider::from_config_in(&cfg.tls, &hostname, Arc::new(FileSecretSource::new()), None)?;
        (Arc::new(DirectTls::new(provider.clone())), Some(provider))
    } else {
        (Arc::new(DirectTcp), None)
    };
    // Behind the edge only: another container on the network must not reach the service directly and skip the edge.
    let accept: Arc<dyn Accept> = if dav.allowed_peers.is_empty() && dav.allowed_hosts.is_empty() {
        accept
    } else if dav.proxy_protocol {
        let named = rusty_svc::NamedPeers::start(dav.allowed_hosts.clone(), rusty_svc::NAMED_PEERS_REFRESH, shutdown.clone()).await;
        Arc::new(rusty_svc::proxy::EdgeAccept::new(dav.allowed_peers.clone(), true).with_named(named))
    } else {
        let named = rusty_svc::NamedPeers::start(dav.allowed_hosts.clone(), rusty_svc::NAMED_PEERS_REFRESH, shutdown.clone()).await;
        Arc::new(PeerAllow::new(accept, dav.allowed_peers.clone()).with_named(named))
    };
    let spec = ListenerSpec { name: "dav", addr: dav.listen, max_connections: MAX_CONNECTIONS, max_per_ip: MAX_PER_IP };
    let bound = BoundListener::bind(spec).await?;
    let addr = bound.local_addr();
    let grace = Duration::from_secs(cfg.limits.shutdown_grace_secs);
    let events: Arc<dyn rusty_svc::ServiceEvents> = Arc::new(TracingEvents);
    let task = tokio::spawn(async move { bound.serve(accept, handler, events, shutdown, grace).await });
    Ok(Running { addr, tls, task })
}
