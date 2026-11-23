//! Start the admin service: the pieces assembled from a configuration and an opened store (the `rusty-admin` binary is a thin `main` over this).
//!
//! * Sessions live in the shared key-value store when `[valkey]` is configured (a restart signs nobody out, several instances share the sessions); without it
//!   they live in this process and a restart signs everyone out. A configured Valkey that cannot be reached **fails the start**: there is no silent fall back
//!   to process memory. The sign-in throttle and the setup-token limiter are still per process.
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
use rusty_svc::{Accept, AuthThrottle, BoundListener, DirectTcp, DirectTls, FileSecretSource, ListenerSpec, PeerAllow, SecretSource, TlsProvider, TracingEvents};
use rusty_valkey::MemoryKv;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::api::{routes, AdminApp, MailSettings, Services};
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

/// Where sessions are kept: the Valkey of `[valkey]` (connected now, so a wrong address or password stops the start), else this process.
async fn session_kv(cfg: &StartupConfig) -> Result<Arc<dyn rusty_valkey::Kv>> {
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
        key_prefix: format!("{}:admin:", v.key_prefix.trim_end_matches(':')),
    })
    .await?;
    tracing::info!(host = %v.host, "admin sessions are kept in Valkey");
    Ok(Arc::new(kv))
}

/// Bind and serve. The listener is bound before this returns (a clash fails the start); `shutdown` drains it.
pub async fn start(cfg: &StartupConfig, store: &Store, shutdown: CancellationToken) -> Result<Running> {
    let Some(admin) = cfg.admin.clone() else { return Err(Error::Invalid("the admin service needs an [admin] section".into())) };
    let repos = store.repos();
    let directory = Directory::with_repo(repos.directory.clone());
    let admins = AdminStore::with_repo(repos.admin.clone());
    let sessions = SessionStore::new(session_kv(cfg).await?, SessionConfig::default());
    let throttle: Arc<dyn rusty_svc::RateLimiter> = match &cfg.valkey {
        // The sessions' store also keeps the sign-in failures, so several instances share the counters.
        Some(_) => Arc::new(rusty_svc::ValkeyThrottle::from_limits(session_kv(cfg).await?, &cfg.limits)),
        None => Arc::new(AuthThrottle::from_limits(&cfg.limits)),
    };
    let authenticator = Arc::new(LocalAuthenticator::new(directory.clone()).await?);
    let auth = AdminAuth::new(authenticator, directory.clone(), admins.clone(), sessions, throttle);

    // The setup page exists only while there is no platform administrator.
    let (gate, setup_token) = if admins.has_platform_admin().await? {
        (None, None)
    } else {
        let (g, t) = SetupGate::new(Arc::new(SystemNow));
        (Some(g), Some(t))
    };
    let sealing = match &cfg.sealing {
        Some(s) => Some(rusty_seal::SealingKey::from_text(&s.public_key)?),
        None => {
            tracing::warn!("no [sealing] section: DKIM keys and relay passwords cannot be created in the web (the endpoints answer 503)");
            None
        }
    };
    let services = Services {
        dns: rusty_store::DnsStore::with_repo(repos.dns.clone()),
        relays: rusty_store::RelayStore::with_repo(repos.relay.clone()),
        blob_stores: rusty_store::BlobStoreAdmin::with_repo(repos.blob_stores.clone()),
        pools: rusty_store::BlobPoolAdmin::with_repos(repos.blob_stores.clone(), repos.blob_pool.clone()),
        sealed: rusty_store::SealedWriter::with_repo(repos.sealed.clone()),
        jobs: rusty_store::JobQueue::with_repo(repos.jobs.clone()),
        monitor: repos.heartbeat.clone(),
        settings: repos.settings.clone(),
        plans: repos.plans.clone(),
        routing: repos.routing.clone(),
        queue: repos.queue.clone(),
        audit: repos.audit.clone(),
        searches: repos.searches.clone(),
        transfers: repos.transfers.clone(),
        sealing,
        mail: cfg.hostname.clone().map(|host| MailSettings { host, public_ips: admin.public_ips.clone(), spf_include: admin.spf_include.clone() }),
    };
    // The heartbeat of the admin web itself (docs/design-admin-monitor.md).
    {
        let beat = rusty_svc::HeartbeatService::new(repos.heartbeat.clone(), "admin", env!("CARGO_PKG_VERSION")).with_detail("{\"listeners\":1}".to_owned());
        let stop = shutdown.clone();
        tokio::spawn(async move { beat.run_until(stop).await });
    }
    let app = Arc::new(AdminApp::new(auth, directory, admins, gate, &admin.public_origin, SessionConfig::default()).with_services(services).with_extra_origins(admin.extra_origins.clone()));
    // What other processes write (the outbound service's checks, another replica, rustyctl) reaches the open pages through the database's announcements.
    crate::live::spawn_feed(app.clone(), store.admin_feed(), shutdown.clone());
    // The older DKIM keys of a rotation end by themselves after their time (docs/design-dkim-rotation.md).
    {
        let (app, jobs, stop) = (app.clone(), rusty_store::JobQueue::with_repo(repos.jobs.clone()), shutdown.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = stop.cancelled() => break,
                    () = tokio::time::sleep(Duration::from_secs(600)) => {}
                }
                if let (Some(s), Ok(now)) = (app.services.as_ref(), jobs.now().await) {
                    match crate::outbound::retire_due_dkim_keys(s, now).await {
                        Ok(n) if n > 0 => tracing::info!(retired = n, "old DKIM keys were retired"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "the scheduled retirement of DKIM keys failed"),
                    }
                }
            }
        });
    }
    let router = routes(app.clone(), Router::new());
    let router = crate::manage::routes(app.clone(), router);
    let router = crate::storage::routes(app.clone(), router);
    let router = crate::outbound::routes(app.clone(), router);
    let router = crate::dns::routes(app.clone(), router);
    let router = crate::admins::routes(app.clone(), router);
    let router = crate::live::routes(app.clone(), router);
    let router = crate::protocols::routes(app.clone(), router);
    let router = crate::plans::routes(app.clone(), router);
    let router = crate::routing::routes(app.clone(), router);
    let router = crate::queue::routes(app.clone(), router);
    let router = crate::import::routes(app.clone(), router);
    let router = crate::settings::routes(app.clone(), router);
    let router = crate::autoconfig::routes(app.clone(), router);
    let router = crate::autodiscover::routes(app.clone(), router);
    let router = crate::monitor::routes(app.clone(), router);
    let router = crate::audit::routes(app.clone(), router);
    let router = crate::search::routes(app.clone(), router);
    let router = crate::certs::routes(app.clone(), router);
    let router = crate::mailrules::routes(app.clone(), router);
    let router = crate::transfers::routes(app.clone(), router);
    let router = crate::audit::record_changes(app.clone(), router);
    // The front end embedded at build time answers every other path (and the single-page routes); without one this is an API server only.
    let router = match crate::web::handler() {
        Some(h) => router.fallback(h),
        None => {
            tracing::warn!("no front end is built into this binary (build web/admin first): only the API is served");
            router
        }
    };
    let handler = Arc::new(HttpHandler::new(router, HttpConfig::default()));

    let (accept, tls): (Arc<dyn Accept>, Option<TlsProvider>) = if cfg.tls.has_cert() {
        let hostname = cfg.hostname.clone().unwrap_or_else(|| "localhost".to_owned());
        let provider = TlsProvider::from_config_in(&cfg.tls, &hostname, Arc::new(FileSecretSource::new()), None)?;
        (Arc::new(DirectTls::new(provider.clone())), Some(provider))
    } else {
        (Arc::new(DirectTcp), None)
    };
    // Behind the edge only: another container on the network must not reach the service directly and skip the edge's address allow-list.
    let accept: Arc<dyn Accept> = if admin.allowed_peers.is_empty() && admin.allowed_hosts.is_empty() {
        accept
    } else if admin.proxy_protocol {
        // The edge says who the client is: the header is read from the allowed peers only, and the address that follows is the client's (docs/design-prost-edge.md).
        let named = rusty_svc::NamedPeers::start(admin.allowed_hosts.clone(), rusty_svc::NAMED_PEERS_REFRESH, shutdown.clone()).await;
        Arc::new(rusty_svc::proxy::EdgeAccept::new(admin.allowed_peers.clone(), true).with_named(named))
    } else {
        let named = rusty_svc::NamedPeers::start(admin.allowed_hosts.clone(), rusty_svc::NAMED_PEERS_REFRESH, shutdown.clone()).await;
        Arc::new(PeerAllow::new(accept, admin.allowed_peers.clone()).with_named(named))
    };
    let spec = ListenerSpec { name: "admin", addr: admin.listen, max_connections: MAX_CONNECTIONS, max_per_ip: MAX_PER_IP };
    let bound = BoundListener::bind(spec).await?;
    let addr = bound.local_addr();
    let grace = Duration::from_secs(cfg.limits.shutdown_grace_secs);
    // `[metrics]`: the counters of the listener and the platform's figures (heartbeats, queue, storage, DNS), on a listener of its own for the allowed scrapers (docs/design-metrics.md).
    let events: Arc<dyn rusty_svc::ServiceEvents> = match &cfg.metrics {
        Some(m) => {
            let registry = Arc::new(rusty_metrics::Registry::new("admin", env!("CARGO_PKG_VERSION")));
            registry.add_collector(Arc::new(crate::monitor::PlatformCollector { monitor: repos.heartbeat.clone(), jobs: rusty_store::JobQueue::with_repo(repos.jobs.clone()) }));
            if !m.scrape.is_empty() {
                registry.set_federation(rusty_metrics::Federation::new(m.scrape.iter().map(|t| rusty_metrics::Target { service: t.service.clone(), address: t.address.clone() }).collect()));
            }
            rusty_metrics::serve_allowing(m.listen, registry.clone(), m.allowed_peers.clone(), m.allowed_hosts.clone(), shutdown.clone()).await?;
            Arc::new(rusty_metrics::MetricsEvents::new(registry))
        }
        None => Arc::new(TracingEvents),
    };
    let task = tokio::spawn(async move { bound.serve(accept, handler, events, shutdown, grace).await });
    Ok(Running { addr, setup_token, tls, task })
}
