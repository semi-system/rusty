//! Start the admin service: the pieces assembled from a configuration and an opened store (the `rusty-admin` binary is a thin `main` over this).
//!
//! * Sessions live in the in-process `Kv` for now: a restart signs everyone out (acceptable for one admin instance, design section 5). A `[valkey]` section
//!   is accepted by the configuration but **not used yet** (the Valkey-backed sessions are a later step); a warning says so.
//! * When no platform administrator exists the setup token is made and **returned**, for the caller to print once; it is never logged.
//! * TLS terminates at the service when `[tls]` names a certificate, otherwise at the edge in front of it (`listen` defaults to loopback).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rusty_core::config::StartupConfig;
use rusty_core::{Error, Result};
use rusty_directory::{Directory, LocalAuthenticator};
use rusty_http::{HttpConfig, HttpHandler, Router};
use rusty_store::{AdminStore, Store};
use rusty_svc::{Accept, AuthThrottle, BoundListener, DirectTcp, DirectTls, FileSecretSource, ListenerSpec, TlsProvider, TracingEvents};
use rusty_valkey::MemoryKv;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::api::{routes, AdminApp};
use crate::auth::AdminAuth;
use crate::session::{SessionConfig, SessionStore, SystemNow};
use crate::setup::SetupGate;

/// Connections the admin listener accepts at once, and from one address: a handful of administrators, not a public service.
const MAX_CONNECTIONS: u32 = 256;
const MAX_PER_IP: u32 = 32;

/// A started admin service.
pub struct Running {
    /// Where it listens (useful when `listen` asked for port 0).
    pub addr: SocketAddr,
    /// The setup token, when no platform administrator exists: print it once, to standard output, and nothing else with it.
    pub setup_token: Option<String>,
    /// The TLS provider, when the service terminates TLS (for SIGHUP reloads).
    pub tls: Option<TlsProvider>,
    pub task: JoinHandle<Result<()>>,
}

/// Bind and serve. The listener is bound before this returns (a clash fails the start); `shutdown` drains it.
pub async fn start(cfg: &StartupConfig, store: &Store, shutdown: CancellationToken) -> Result<Running> {
    let Some(admin) = cfg.admin.clone() else { return Err(Error::Invalid("the admin service needs an [admin] section".into())) };
    if cfg.valkey.is_some() {
        tracing::warn!("[valkey] is configured but the admin sessions are kept in this process for now: a restart signs everyone out");
    }
    let repos = store.repos();
    let directory = Directory::with_repo(repos.directory.clone());
    let admins = AdminStore::with_repo(repos.admin.clone());
    let sessions = SessionStore::new(Arc::new(MemoryKv::new()), SessionConfig::default());
    let throttle = Arc::new(AuthThrottle::from_limits(&cfg.limits));
    let authenticator = Arc::new(LocalAuthenticator::new(directory.clone()).await?);
    let auth = AdminAuth::new(authenticator, directory.clone(), admins.clone(), sessions, throttle);

    // The setup page exists only while there is no platform administrator.
    let (gate, setup_token) = if admins.has_platform_admin().await? {
        (None, None)
    } else {
        let (g, t) = SetupGate::new(Arc::new(SystemNow));
        (Some(g), Some(t))
    };
    let app = Arc::new(AdminApp::new(auth, directory, admins, gate, &admin.public_origin, SessionConfig::default()));
    let handler = Arc::new(HttpHandler::new(routes(app, Router::new()), HttpConfig::default()));

    let (accept, tls): (Arc<dyn Accept>, Option<TlsProvider>) = if cfg.tls.has_cert() {
        let hostname = cfg.hostname.clone().unwrap_or_else(|| "localhost".to_owned());
        let provider = TlsProvider::from_config_in(&cfg.tls, &hostname, Arc::new(FileSecretSource::new()), None)?;
        (Arc::new(DirectTls::new(provider.clone())), Some(provider))
    } else {
        (Arc::new(DirectTcp), None)
    };
    let spec = ListenerSpec { name: "admin", addr: admin.listen, max_connections: MAX_CONNECTIONS, max_per_ip: MAX_PER_IP };
    let bound = BoundListener::bind(spec).await?;
    let addr = bound.local_addr();
    let grace = Duration::from_secs(cfg.limits.shutdown_grace_secs);
    let task = tokio::spawn(async move { bound.serve(accept, handler, Arc::new(TracingEvents), shutdown, grace).await });
    Ok(Running { addr, setup_token, tls, task })
}
