//! The all-in-one daemon: start-up checks, store and TLS, the listeners, and graceful shutdown.
//!
//! This is the only place that opens a `Store`; everything it hands to the services is a facade or
//! a trait object (`Context`).

use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::sync::Arc;
use std::time::Duration;

use rusty_core::config::{Listen, Profile, Role, StartupConfig};
use rusty_core::{Error, Result};
use rusty_store::{BlobStore, FsBlobStore, Store};
use rusty_svc::{
    listener_plan_for, run_services, Accept, BoundListener, Context, DirectTcp, DirectTls, FileSecretSource,
    ChangeFanout, ChangeFeedService, GcService, ListenerKind, ListenerService, Service, SessionHandler, SystemClock,
    TlsProvider,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use rusty_smtp::inbound::{InboundHandler, InboundOptions, SubmissionDeps};
use rusty_smtp::limits::{MemoryBudget, MemorySubmissionLimiter};
use rusty_smtp::dkim::keyring;
use rusty_smtp::outbound::dial::TcpDialer;
use rusty_smtp::outbound::route::ConfigRoute;
use rusty_smtp::outbound::tls::RustlsUpgrader;
use rusty_smtp::outbound::worker::{OutboundWorker, Wake, WorkerDeps, WorkerOptions};
use rusty_smtp::outbound_queue::{JobOutbound, SubmitLimits};
use rusty_smtp::stack::DnsStack;


/// Messages one account may submit per hour (U4 default; becomes a config key with per-tenant limits).
const SUBMISSIONS_PER_HOUR: u32 = 200;
/// Messages per submission connection (mail clients reuse one connection for a whole outbox).
const SUBMISSION_TRANSACTIONS: u32 = 100;

/// How often the GC pass runs.
const GC_EVERY: Duration = Duration::from_secs(3600);
/// How often every blob store gets its upkeep (a pool: missing copies, missed deletions, a draining backend).
const MAINTAIN_EVERY: Duration = Duration::from_secs(60);

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
    /// the store (the connection pool).
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
pub(crate) async fn prepare_dev_dirs(cfg: &StartupConfig) -> Result<()> {
    let dir = cfg
        .blob_dir
        .parent()
        .ok_or_else(|| Error::Invalid("blob_dir has no directory".into()))?;
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

/// Check the configuration, open the store and TLS, bind every listener, and start serving.
/// Nothing is left half-started: on any error the store is closed and no listener runs. A refused
/// configuration touches no file.
pub async fn start(cfg: StartupConfig, profile: Profile, shutdown: CancellationToken) -> Result<Running> {
    start_role(cfg, profile, None, shutdown).await
}

/// [`start`] for one service of the platform (`Some(role)`: only its listeners and background services) or for the all-in-one daemon (`None`).
pub async fn start_role(cfg: StartupConfig, profile: Profile, role: Option<Role>, shutdown: CancellationToken) -> Result<Running> {
    cfg.check_startup_for(profile, role)?;
    let hostname = cfg.hostname.clone().unwrap_or_else(|| "localhost".to_owned());
    // Dev profile: the data directory first, because the dev certificate is kept in it.
    if profile == Profile::Dev {
        prepare_dev_dirs(&cfg).await?;
    }
    // TLS first: a bad certificate or key file must not get as far as opening the database.
    let tls = if cfg.tls.has_cert() || cfg.tls.dev_self_signed {
        TlsProvider::from_config_in(&cfg.tls, &hostname, Arc::new(FileSecretSource::new()), cfg.blob_dir.parent())?
    } else if cfg.tls_required_listeners_for(role) == 0 {
        // check_startup already refused every configuration with an authenticated listener.
        tracing::warn!("no TLS configured: port 25 will not offer STARTTLS");
        TlsProvider::unavailable()?
    } else {
        return Err(Error::Invalid("TLS is not configured".into()));
    };
    for w in cfg.startup_warnings(profile) {
        tracing::warn!("{w}");
    }
    let store = Store::open(rusty_runtime::store::store_config(&cfg, profile)?).await?;
    match build(&cfg, &store, tls.clone(), role, shutdown.clone()).await {
        Ok((addrs, task)) => Ok(Running { addrs, shutdown, tls, store, task }),
        Err(e) => {
            let _ = store.close().await;
            Err(e)
        }
    }
}

async fn build(
    cfg: &StartupConfig,
    store: &Store,
    tls: TlsProvider,
    role: Option<Role>,
    shutdown: CancellationToken,
) -> Result<(Vec<(ListenerKind, SocketAddr)>, JoinHandle<Result<()>>)> {
    // What this process runs: everything for the all-in-one daemon, else the one service.
    let runs = |r: Role| role.is_none_or(|x| x == r);
    let hostname = cfg.hostname.clone().unwrap_or_else(|| "localhost".to_owned());
    let hostname = hostname.as_str();
    let blobs = platform_blobs(cfg).await?;
    let cfg = Arc::new(cfg.clone());
    let (opener, s3_opener, backend_opener) = tenant_store_opener(&cfg, store)?;
    let mut ctx = Context::new_with_opener(store.repos(), blobs, cfg.clone(), tls.clone(), opener).await?;
    // The services that check passwords count failures in the shared store (every replica, and submission with IMAP, against one counter); the in-memory throttle stays under it.
    let auth_kv = rusty_runtime::kv::connect_kv(&cfg, "auth").await?;
    if let Some(kv) = &auth_kv {
        ctx = ctx.with_rate(Arc::new(rusty_svc::ValkeyThrottle::from_limits(kv.clone(), &cfg.limits)));
    }

    // Bind everything before serving anything: a clash fails the whole start.
    let mut addrs = Vec::new();
    let mut services: Vec<Box<dyn Service>> = Vec::new();
    // One memory budget for every SMTP listener of the process.
    let budget = MemoryBudget::new_mib(cfg.limits.inflight_message_mib);
    // The outbound side: the DNS stack (MX lookups and inbound SPF/DKIM/DMARC), the worker and the wake-up
    // that lets a fresh submission skip the poll delay. Without it, submission refuses remote recipients.
    let wake = Wake::default();
    // The resolver stack serves the inbound checks (smtp-in) and the worker (outbound); no other service needs it.
    let dns = if cfg.outbound.enabled && (runs(Role::SmtpIn) || runs(Role::Outbound)) { Some(DnsStack::from_config(&cfg.outbound, hostname)?) } else { None };
    let secrets = Arc::new(FileSecretSource::new());
    // The scanners of the inbound listener (`[filter]`): the virus scan first (a virus is refused before anything else is asked), then the spam scorer.
    let mut inbound_filters: Vec<Arc<dyn rusty_smtp::filter::InboundFilter>> = Vec::new();
    if let Some(f) = &cfg.filter {
        let max = f.max_scan_bytes;
        if let Some(addr) = &f.clamd_addr {
            inbound_filters.push(Arc::new(rusty_smtp::scan::ClamdFilter::new(addr, Duration::from_secs(f.clamd_timeout_secs), max, f.clamd_on_error)));
        }
        if let Some(url) = &f.rspamd_url {
            inbound_filters.push(Arc::new(rusty_smtp::scan::RspamdFilter::new(url, Duration::from_secs(f.rspamd_timeout_secs), max, f.rspamd_on_error)));
        }
    }
    for planned in listener_plan_for(&cfg, role) {
        let bound = BoundListener::bind(planned.spec).await?;
        addrs.push((planned.kind, bound.local_addr()));
        // Behind the edge (PROXY v2 from a trusted proxy) the client address comes from the header and, on an implicit-TLS port, TLS ended at the edge;
        // otherwise the service accepts the connection itself.
        let (proxied, trusted) = if planned.kind.is_imap() {
            (cfg.imap.proxy_protocol, &cfg.imap.trusted_proxies)
        } else {
            (cfg.smtp.proxy_protocol, &cfg.smtp.trusted_proxies)
        };
        let accept: Arc<dyn Accept> = if proxied {
            Arc::new(rusty_svc::proxy::EdgeAccept::new(trusted.clone(), planned.kind.implicit_tls()))
        } else if planned.kind.implicit_tls() {
            Arc::new(DirectTls::new(tls.clone()))
        } else {
            Arc::new(DirectTcp)
        };
        let handler: Arc<dyn SessionHandler> = match planned.kind {
            ListenerKind::Smtp25 => {
                Arc::new(InboundHandler::new(
                    &ctx,
                    InboundOptions { budget: budget.clone(), auth: dns.as_ref().map(|d| d.checker.clone()), filters: inbound_filters.clone(), ..InboundOptions::default() },
                ))
            }
            ListenerKind::Submission587 | ListenerKind::Submissions465 => {
                let deps = SubmissionDeps {
                    outbound: Arc::new(
                        JobOutbound::new(ctx.jobs.clone(), ctx.directory.clone())
                            .with_limits(SubmitLimits::from_config(&cfg.outbound))
                            .with_wake(wake.clone()),
                    ),
                    // With `[valkey]` the sending budget of an account is one for every replica of the service; without it, per process.
                    limiter: match &auth_kv {
                        Some(kv) => Arc::new(rusty_smtp::limits::ValkeySubmissionLimiter::new(kv.clone(), SUBMISSIONS_PER_HOUR, Arc::new(SystemClock::new()))),
                        None => Arc::new(MemorySubmissionLimiter::new(SUBMISSIONS_PER_HOUR, Arc::new(SystemClock::new()))),
                    },
                    remote_enabled: cfg.outbound.enabled,
                };
                let options = InboundOptions {
                    max_transactions: SUBMISSION_TRANSACTIONS,
                    budget: budget.clone(),
                    ..InboundOptions::default()
                };
                Arc::new(InboundHandler::submission(&ctx, options, deps))
            }
            ListenerKind::Imap143 | ListenerKind::Imaps993 => Arc::new(rusty_imap::ImapHandler::new(ctx.clone()).with_budget(budget.clone())),
        };
        services.push(Box::new(ListenerService::new(bound, accept, handler)));
    }
    if runs(Role::Gc) {
        services.push(Box::new(GcService::new(GC_EVERY)));
        services.push(Box::new(rusty_svc::MaintainService::new(MAINTAIN_EVERY)));
    }
    // Changes made by OTHER processes (a delivery by smtp-in, a job queued by submission): the database announces them
    // at commit (migration 0017) and this one LISTEN connection hands them to the hub (IMAP IDLE) and the outbound
    // worker. In this all-in-one process it only repeats what the in-memory path already did; it is here so that the
    // path every split deployment depends on runs in every start-up. Notifications are hints: IDLE and the worker
    // keep their timers.
    // Only the services that wait for those changes listen: imap (IDLE) and outbound (the worker).
    if runs(Role::Imap) || runs(Role::Outbound) {
        let changes = ChangeFanout::new(ctx.hub.clone()).with_jobs(wake.0.clone());
        services.push(Box::new(ChangeFeedService::new(store.change_feed(), Arc::new(changes))));
    }
    let outbound_dns = if runs(Role::Outbound) { dns } else { None };
    if let Some(dns) = outbound_dns {
        let deps = WorkerDeps {
            resolver: dns.resolver.clone(),
            dialer: Arc::new(TcpDialer),
            tls: Arc::new(RustlsUpgrader::new(cfg.outbound.ca_bundle.as_deref())?),
            route: route_of(&cfg, store, &ctx, secrets.as_ref())?,
            signer: signer_of(&cfg, store, &ctx, secrets.as_ref())?,
        };
        // The check of a tenant's relay (`relay.check`) runs here, where the unsealing key is: only with `[unsealing]` can a relay password be opened.
        if cfg.unsealing.is_some() {
            services.push(Box::new(rusty_smtp::outbound::relay_job::RelayCheckWorker::new(
                rusty_store::RelayStore::with_repo(store.repos().relay.clone()),
                deps.route.clone(),
                cfg.outbound.clone(),
                deps.resolver.clone(),
                deps.dialer.clone(),
                deps.tls.clone(),
                hostname,
            )));
        }
        // The check of a domain's DNS records (`dns.check`, queued by the admin web): it queries DNS, so it runs where the resolver settings are.
        let mut dns_checks = rusty_smtp::outbound::dns_job::DnsCheckWorker::new(
            rusty_store::DnsStore::with_repo(store.repos().dns.clone()),
            cfg.outbound.clone(),
            hostname,
            // `[dns_check] nameserver` (the operator's choice for these checks) wins over the outbound resolver.
            Arc::new(rusty_smtp::stack::LiveDns::with_nameserver(cfg.dns_check.nameserver.map(|n| n.addr()).or(cfg.outbound.nameserver))?),
        );
        // `[dns_check] second_nameserver`: every check is made against both resolvers and a disagreement is propagation lag (`pending`), not a fault.
        if let Some(second) = cfg.dns_check.second_nameserver {
            dns_checks = dns_checks.with_second(Arc::new(rusty_smtp::stack::LiveDns::with_nameserver(Some(second.addr()))?));
        }
        services.push(Box::new(dns_checks));
        services.push(Box::new(OutboundWorker::new(deps, WorkerOptions::new(cfg.outbound.clone(), hostname)).with_wake(wake)));
    }
    // The periodic queueing of `dns.check` jobs belongs to the service of the periodic jobs (gc); the worker that runs them is the outbound one, where the resolver is.
    if runs(Role::Gc) && cfg.dns_check.enabled {
        services.push(Box::new(rusty_smtp::outbound::dns_job::DnsCheckScheduler::new(
            rusty_store::DnsStore::with_repo(store.repos().dns.clone()),
            cfg.dns_check.interval_secs,
            cfg.dns_check.jitter_secs,
        )));
    }
    // The check of a tenant's S3 store (`storage.check`, queued by the admin web or `rustyctl`): it needs the unsealing key, so it runs only where the S3 opener exists.
    if let (true, Some(s3_opener)) = (runs(Role::Outbound), s3_opener) {
        services.push(Box::new(
            rusty_s3::StorageCheckWorker::new(
                rusty_store::BlobStoreAdmin::with_repo(store.repos().blob_stores.clone()),
                Arc::new(rusty_s3::S3Prober::new(s3_opener).with_other_backends(backend_opener)),
            )
            .with_pools(rusty_store::BlobPoolAdmin::with_repos(store.repos().blob_stores.clone(), store.repos().blob_pool.clone())),
        ));
    }
    for (kind, addr) in &addrs {
        tracing::info!(service = kind.name(), %addr, "listening");
    }
    let task = tokio::spawn(run_services(services, ctx, shutdown));
    Ok((addrs, task))
}

/// The opener of the blob stores of tenants: directories next to the platform's, and, when `[unsealing]` names this service's private key (the credentials of an S3 store are sealed
/// to it), S3 buckets. Without the key a tenant's S3 store cannot be opened, so it is not offered.
/// The platform blob store: the bucket of `[blob_s3]` when it is set, else the directory `blob_dir`.
pub(crate) async fn platform_blobs(cfg: &StartupConfig) -> Result<Arc<dyn BlobStore>> {
    match &cfg.blob_s3 {
        Some(s3) => Ok(Arc::new(rusty_s3::platform_store(s3, &FileSecretSource::new())?)),
        None => Ok(Arc::new(FsBlobStore::open(&cfg.blob_dir).await?)),
    }
}

/// The openers of tenants' blob stores: all kinds served (for the registry), and the S3 one by itself (the store check needs it), present only with an unsealing key.
type TenantOpeners = (Arc<dyn rusty_store::StoreOpener>, Option<Arc<rusty_s3::S3StoreOpener>>, Arc<dyn rusty_store::BackendOpener>);

pub(crate) fn tenant_store_opener(cfg: &StartupConfig, store: &Store) -> Result<TenantOpeners> {
    let stores_dir = cfg.blob_dir.parent().map_or_else(|| std::path::PathBuf::from("stores"), |p| p.join("stores"));
    let fs = Arc::new(rusty_store::FsStoreOpener::new(stores_dir));
    let mut opener = rusty_store::KindOpener::new().with_fs(fs.clone());
    let mut backends = rusty_store::KindBackendOpener::new().with_fs(fs);
    let mut s3 = None;
    if let Some(unsealer) = unsealer_of(cfg, &FileSecretSource::new())? {
        let tls = rusty_s3::client::tls_config(None)?;
        let reader = rusty_store::SealedReader::with_repo(store.repos().sealed.clone());
        let o = Arc::new(rusty_s3::S3StoreOpener::new(reader, Arc::new(unsealer), tls));
        opener = opener.with_s3(o.clone());
        backends = backends.with_s3(o.clone());
        s3 = Some(o);
    }
    // A pool opens its backends with the backend opener (a bucket needs the unsealing key like a single S3 store does).
    let backends: Arc<dyn rusty_store::BackendOpener> = Arc::new(backends);
    opener = opener.with_pool(Arc::new(rusty_store::PoolOpener::new(store.repos().blob_pool.clone(), backends.clone())));
    Ok((Arc::new(opener), s3, backends))
}

/// The private keys named by `[unsealing]` (none when the section is absent: this service opens no sealed secret).
fn unsealer_of(cfg: &StartupConfig, secrets: &dyn rusty_svc::SecretSource) -> Result<Option<rusty_seal::Unsealer>> {
    let Some(u) = &cfg.unsealing else { return Ok(None) };
    let mut keys = Vec::new();
    for path in &u.key_files {
        keys.push(rusty_seal::UnsealingKey::from_file_bytes(secrets.read(path)?)?);
    }
    Ok(Some(rusty_seal::Unsealer::new(keys)?))
}

/// The worker's route: the platform's (`[outbound.relay]` or direct) alone, or, when `[unsealing]` is set, the relay a domain's tenant entered in the
/// web with the platform's as the fallback. Without an unsealing key a tenant relay could not be used (its password cannot be opened), so it is not offered.
fn route_of(cfg: &StartupConfig, store: &Store, ctx: &Context, secrets: &dyn rusty_svc::SecretSource) -> Result<Arc<dyn rusty_smtp::outbound::route::OutboundRoute>> {
    let platform: Arc<dyn rusty_smtp::outbound::route::OutboundRoute> = Arc::new(ConfigRoute::from_config(&cfg.outbound, secrets)?);
    let Some(unsealer) = unsealer_of(cfg, secrets)? else { return Ok(platform) };
    let repos = store.repos();
    Ok(Arc::new(rusty_smtp::outbound::domain_route::DomainRoute::new(
        ctx.directory.clone(),
        rusty_store::RelayStore::with_repo(repos.relay.clone()),
        rusty_store::SealedReader::with_repo(repos.sealed.clone()),
        unsealer,
        platform,
    )))
}

/// The worker's DKIM signer: the sealed keys of the database when `[unsealing]` names this service's private key files, the key files of
/// `[outbound] dkim_dir` for domains that have no sealed key; none when neither is configured (mail goes unsigned).
fn signer_of(
    cfg: &StartupConfig,
    store: &Store,
    ctx: &Context,
    secrets: &dyn rusty_svc::SecretSource,
) -> Result<Option<Arc<dyn rusty_smtp::dkim::MessageSigner>>> {
    let files = keyring(&cfg.outbound, Arc::new(FileSecretSource::new()));
    let sealed = unsealer_of(cfg, secrets)?.map(|unsealer| {
        let repos = store.repos();
        Arc::new(rusty_smtp::dkim::SealedKeyring::new(
            ctx.directory.clone(),
            rusty_store::DnsStore::with_repo(repos.dns.clone()),
            rusty_store::SealedReader::with_repo(repos.sealed.clone()),
            unsealer,
        ))
    });
    Ok((sealed.is_some() || files.is_some()).then(|| rusty_smtp::dkim::chain(sealed, files)))
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
