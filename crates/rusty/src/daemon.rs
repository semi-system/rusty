//! The all-in-one daemon: start-up checks, store and TLS, the listeners, and graceful shutdown.
//!
//! This is the only place that opens a `Store`; everything it hands to the services is a facade or
//! a trait object (`Context`).

use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rusty_core::config::{Listen, Profile, StartupConfig};
use rusty_core::{Error, Result};
use rusty_store::{BlobStore, FsBlobStore, Store, StoreConfig};
use rusty_svc::{
    listener_plan, run_services, Accept, BoundListener, Context, DirectTcp, DirectTls, FileSecretSource,
    GcService, ListenerKind, ListenerService, Service, TlsProvider,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::banner::BannerSession;

/// How often the GC pass runs.
const GC_EVERY: Duration = Duration::from_secs(3600);

/// A started daemon: listeners are bound and serving.
pub struct Running {
    /// The address each listener actually bound (useful with port 0).
    pub addrs: Vec<(ListenerKind, SocketAddr)>,
    shutdown: CancellationToken,
    tls: TlsProvider,
    store: Store,
    task: JoinHandle<Result<()>>,
}

impl Running {
    /// Ask the services to stop (same as cancelling the token given to [`start`]).
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    /// The TLS provider, for the SIGHUP reload.
    pub fn tls(&self) -> &TlsProvider {
        &self.tls
    }

    /// Wait for every service to finish (sessions drain up to the configured grace), then close
    /// the store: the SQLite WAL is checkpointed before the process exits.
    pub async fn wait(self) -> Result<()> {
        let served = match self.task.await {
            Ok(r) => r,
            Err(_) => Err(Error::storage("the service runner panicked")),
        };
        let closed = self.store.close().await;
        served.and(closed)
    }
}

/// Dev profile only: the data directory is created 0700 (or must already be private and not a
/// symlink) and the blob root is initialised. Production never creates anything implicitly.
async fn prepare_dev_dirs(cfg: &StartupConfig) -> Result<()> {
    let dir = cfg
        .database_path
        .parent()
        .ok_or_else(|| Error::Invalid("database_path has no directory".into()))?;
    match std::fs::symlink_metadata(dir) {
        Ok(m) => {
            if !m.is_dir() {
                return Err(Error::Invalid("the dev data dir is not a directory (symlinks are refused)".into()));
            }
            if m.permissions().mode() & 0o077 != 0 {
                return Err(Error::Invalid("the dev data dir must be private (mode 0700)".into()));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .map_err(|e| Error::storage_with("cannot create the dev data dir", e))?;
        }
        Err(e) => return Err(Error::storage_with("cannot inspect the dev data dir", e)),
    }
    FsBlobStore::init(&cfg.blob_dir).await
}

fn sqlite_url(path: &Path) -> Result<String> {
    let s = path.to_str().ok_or_else(|| Error::Invalid("database_path is not valid UTF-8".into()))?;
    if s.contains(['?', '#', '%']) {
        return Err(Error::Invalid("database_path must not contain '?', '#' or '%'".into()));
    }
    Ok(format!("sqlite://{s}"))
}

/// Check the configuration, open the store and TLS, bind every listener, and start serving.
/// Nothing is left half-started: on any error the store is closed and no listener runs. A refused
/// configuration touches no file.
pub async fn start(cfg: StartupConfig, profile: Profile, shutdown: CancellationToken) -> Result<Running> {
    cfg.check_startup(profile)?;
    let hostname = cfg.hostname.clone().unwrap_or_else(|| "localhost".to_owned());
    // TLS first: a bad certificate or key file must not get as far as opening the database.
    let tls = if cfg.tls.has_cert() || cfg.tls.dev_self_signed {
        TlsProvider::from_config(&cfg.tls, &hostname, Arc::new(FileSecretSource::new()))?
    } else if cfg.authenticated_listeners() == 0 {
        // check_startup already refused every configuration with an authenticated listener.
        tracing::warn!("no TLS configured: port 25 will not offer STARTTLS");
        TlsProvider::unavailable()?
    } else {
        return Err(Error::Invalid("TLS is not configured".into()));
    };
    if profile == Profile::Dev {
        prepare_dev_dirs(&cfg).await?;
    }
    let store = Store::open(StoreConfig::sqlite(sqlite_url(&cfg.database_path)?)).await?;
    match build(&cfg, &hostname, &store, tls.clone(), shutdown.clone()).await {
        Ok((addrs, task)) => Ok(Running { addrs, shutdown, tls, store, task }),
        Err(e) => {
            let _ = store.close().await;
            Err(e)
        }
    }
}

async fn build(
    cfg: &StartupConfig,
    hostname: &str,
    store: &Store,
    tls: TlsProvider,
    shutdown: CancellationToken,
) -> Result<(Vec<(ListenerKind, SocketAddr)>, JoinHandle<Result<()>>)> {
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::open(&cfg.blob_dir).await?);
    let cfg = Arc::new(cfg.clone());
    let ctx = Context::new(store.repos(), blobs, cfg.clone(), tls.clone()).await?;

    // Bind everything before serving anything: a clash fails the whole start.
    let mut addrs = Vec::new();
    let mut services: Vec<Box<dyn Service>> = Vec::new();
    for planned in listener_plan(&cfg) {
        let bound = BoundListener::bind(planned.spec).await?;
        addrs.push((planned.kind, bound.local_addr()));
        let accept: Arc<dyn Accept> = if planned.kind.implicit_tls() {
            Arc::new(DirectTls::new(tls.clone()))
        } else {
            Arc::new(DirectTcp)
        };
        let handler: Arc<dyn rusty_svc::SessionHandler> = if planned.kind.is_imap() {
            Arc::new(rusty_imap::ImapHandler::new(ctx.clone()))
        } else {
            Arc::new(BannerSession::new(planned.kind, hostname))
        };
        services.push(Box::new(ListenerService::new(bound, accept, handler)));
    }
    services.push(Box::new(GcService::new(GC_EVERY)));
    for (kind, addr) in &addrs {
        tracing::info!(service = kind.name(), %addr, "listening");
    }
    let task = tokio::spawn(run_services(services, ctx, shutdown));
    Ok((addrs, task))
}

/// Move every dev listener by `offset` ports (several dev instances, tests).
pub fn with_port_offset(mut cfg: StartupConfig, offset: u16) -> Result<StartupConfig> {
    let shift = |l: Listen| -> Result<Listen> {
        match l.addr() {
            None => Ok(l),
            Some(mut a) => {
                let p = a.port().checked_add(offset).ok_or_else(|| Error::Invalid("port offset is too large".into()))?;
                a.set_port(p);
                Ok(Listen::on(a))
            }
        }
    };
    cfg.smtp.listen_25 = shift(cfg.smtp.listen_25)?;
    cfg.smtp.listen_587 = shift(cfg.smtp.listen_587)?;
    cfg.smtp.listen_465 = shift(cfg.smtp.listen_465)?;
    cfg.imap.listen_143 = shift(cfg.imap.listen_143)?;
    cfg.imap.listen_993 = shift(cfg.imap.listen_993)?;
    Ok(cfg)
}

/// Put every dev listener on `ip` (the dev profile binds 127.0.0.1; a container needs 0.0.0.0).
pub fn with_bind(mut cfg: StartupConfig, ip: std::net::IpAddr) -> StartupConfig {
    let on = |l: Listen| match l.addr() {
        None => l,
        Some(mut a) => {
            a.set_ip(ip);
            Listen::on(a)
        }
    };
    cfg.smtp.listen_25 = on(cfg.smtp.listen_25);
    cfg.smtp.listen_587 = on(cfg.smtp.listen_587);
    cfg.smtp.listen_465 = on(cfg.smtp.listen_465);
    cfg.imap.listen_143 = on(cfg.imap.listen_143);
    cfg.imap.listen_993 = on(cfg.imap.listen_993);
    cfg
}
