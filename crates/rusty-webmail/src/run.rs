//! Start the webmail service: the pieces assembled from a configuration and an opened store (the `rusty-webmail` binary is a thin `main` over this).
//!
//! * Sessions live in the shared key-value store when `[valkey]` is configured (a restart signs nobody out, several instances share the sessions); without it they live in this process and a restart signs
//!   everyone out. A configured Valkey that cannot be reached **fails the start**: there is no silent fall back to process memory.
//! * TLS terminates at the service when `[tls]` names a certificate, otherwise at the edge in front of it (`listen` defaults to loopback). Behind the edge only: `allowed_peers` and `allowed_hosts` drop
//!   every other peer before a byte is read, and `proxy_protocol` takes the client's address from the edge's header.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rusty_core::config::StartupConfig;
use rusty_core::{Error, Result};
use rusty_directory::{Directory, LocalAuthenticator};
use rusty_http::{HttpConfig, HttpHandler, Router};
use rusty_store::{BlobRouter, BlobStore, BlobStores, FsBlobStore, FsStoreOpener, KindOpener, MailboxLimits, Store};
use rusty_svc::{MailboxHub, Accept, AuthThrottle, BoundListener, DirectTcp, DirectTls, FileSecretSource, ListenerSpec, PeerAllow, SecretSource, TlsProvider, TracingEvents};
use rusty_valkey::MemoryKv;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::api::{routes, WebmailApp, WebmailAuth};
use crate::session::{SessionConfig, SessionStore};

/// Connections the listener accepts at once, and from one address (an office behind one address is many people).
const MAX_CONNECTIONS: u32 = 1024;
const MAX_PER_IP: u32 = 128;

/// A started webmail service.
pub struct Running {
    /// Where it listens (useful when `listen` asked for port 0).
    pub addr: SocketAddr,
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
        key_prefix: format!("{}:webmail:", v.key_prefix.trim_end_matches(':')),
    })
    .await?;
    tracing::info!(host = %v.host, "webmail sessions are kept in Valkey");
    Ok(Arc::new(kv))
}

/// Bind and serve. The listener is bound before this returns (a clash fails the start); `shutdown` drains it.
pub async fn start(cfg: &StartupConfig, store: &Store, shutdown: CancellationToken) -> Result<Running> {
    let Some(webmail) = cfg.webmail.clone() else { return Err(Error::Invalid("the webmail service needs a [webmail] section".into())) };
    let repos = store.repos();
    let directory = Directory::with_repo(repos.directory.clone());
    let kv = session_kv(cfg).await?;
    let sessions = SessionStore::new(kv.clone(), SessionConfig::default());
    let throttle: Arc<dyn rusty_svc::RateLimiter> = match &cfg.valkey {
        // The sessions' store also keeps the sign-in failures, so several instances share the counters.
        Some(_) => Arc::new(rusty_svc::ValkeyThrottle::from_limits(kv, &cfg.limits)),
        None => Arc::new(AuthThrottle::from_limits(&cfg.limits)),
    };
    let authenticator = Arc::new(LocalAuthenticator::new(directory.clone()).await?);
    let second = crate::second_factor::SecondFactor::new(repos.totp.clone(), webmail.totp_key_file.as_deref(), &FileSecretSource::new())?;
    let auth = WebmailAuth::new(authenticator, directory.clone(), sessions, throttle, second);
    // The mail: the platform blob store (a filesystem directory, or the S3 bucket of `[blob_s3]`); a domain's own store needs a key this service does not hold, so its messages answer as unavailable.
    let blobs: Arc<dyn BlobStore> = match &cfg.blob_s3 {
        Some(s3) => Arc::new(rusty_s3::platform_store(s3, &FileSecretSource::new())?),
        None => Arc::new(FsBlobStore::open(&cfg.blob_dir).await?),
    };
    let stores_dir = cfg.blob_dir.parent().map_or_else(|| std::path::PathBuf::from("stores"), |p| p.join("stores"));
    let opener = Arc::new(KindOpener::new().with_fs(Arc::new(FsStoreOpener::new(stores_dir))));
    let stores: Arc<dyn BlobRouter> = Arc::new(BlobStores::new(blobs.clone(), repos.blob_stores.clone(), opener));
    let limits = MailboxLimits { max_message_bytes: usize::try_from(cfg.max_message_bytes).unwrap_or(usize::MAX), quota_bytes: None };
    let limits_max = limits.max_message_bytes;
    let hub = MailboxHub::new(repos.mailbox.clone(), stores, limits).with_directory(directory.clone());
    let app = Arc::new(WebmailApp { auth, hub, blobs, outbox: repos.outbox.clone(), app_passwords: repos.app_passwords.clone(), sieve: repos.sieve.clone(), dav: repos.dav.clone(), settings: repos.webmail_settings.clone(), max_message_bytes: limits_max, public_origin: webmail.public_origin.clone(), extra_origins: webmail.extra_origins.clone() });

    // The heartbeat of the webmail itself, for the Monitor page of the admin web.
    {
        let beat = rusty_svc::HeartbeatService::new(repos.heartbeat.clone(), "webmail", env!("CARGO_PKG_VERSION")).with_detail("{\"listeners\":1}".to_owned());
        let stop = shutdown.clone();
        tokio::spawn(async move { beat.run_until(stop).await });
    }
    let router = routes(app.clone(), Router::new());
    let router = crate::read::routes(app.clone(), router);
    let router = crate::write::routes(app.clone(), router);
    let router = crate::send::routes(app.clone(), router);
    let router = crate::apppw::routes(app.clone(), router);
    let router = crate::settings::routes(app.clone(), router);
    let router = crate::second::routes(app.clone(), router);
    let router = crate::filters::routes(app.clone(), router);
    let router = crate::contacts::routes(app, router);
    // The front end embedded at build time answers every other path; without one this is an API server only.
    let router = match crate::web::handler() {
        Some(h) => router.fallback(h),
        None => {
            tracing::warn!("no front end is built into this binary (build web/webmail first): only the API is served");
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
    // Behind the edge only: another container on the network must not reach the service directly and skip the edge.
    let accept: Arc<dyn Accept> = if webmail.allowed_peers.is_empty() && webmail.allowed_hosts.is_empty() {
        accept
    } else if webmail.proxy_protocol {
        let named = rusty_svc::NamedPeers::start(webmail.allowed_hosts.clone(), rusty_svc::NAMED_PEERS_REFRESH, shutdown.clone()).await;
        Arc::new(rusty_svc::proxy::EdgeAccept::new(webmail.allowed_peers.clone(), true).with_named(named))
    } else {
        let named = rusty_svc::NamedPeers::start(webmail.allowed_hosts.clone(), rusty_svc::NAMED_PEERS_REFRESH, shutdown.clone()).await;
        Arc::new(PeerAllow::new(accept, webmail.allowed_peers.clone()).with_named(named))
    };
    let spec = ListenerSpec { name: "webmail", addr: webmail.listen, max_connections: MAX_CONNECTIONS, max_per_ip: MAX_PER_IP };
    let bound = BoundListener::bind(spec).await?;
    let addr = bound.local_addr();
    let grace = Duration::from_secs(cfg.limits.shutdown_grace_secs);
    let events: Arc<dyn rusty_svc::ServiceEvents> = Arc::new(TracingEvents);
    let task = tokio::spawn(async move { bound.serve(accept, handler, events, shutdown, grace).await });
    Ok(Running { addr, tls, task })
}
