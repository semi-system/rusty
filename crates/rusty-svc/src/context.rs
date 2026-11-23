//! `Context`: everything a service is built from. The composition root (`rusty`) is the only
//! place that opens a `Store`; it passes the repositories here. Services hold trait objects and
//! facades, so a `store` service or another backend needs no service change.

use std::sync::Arc;

use rusty_core::config::StartupConfig;
use rusty_core::Result;
use rusty_directory::{Authenticator, Directory, LocalAuthenticator};
use rusty_store::{BlobRouter, BlobStore, BlobStores, FsStoreOpener, Gc, JobQueue, KindOpener, MailboxLimits, Repos, StoreOpener};

use crate::events::{ServiceEvents, TracingEvents};
use crate::hub::MailboxHub;
use crate::quota::{QuotaPolicy, SizeCap};
use crate::protocols::ProtocolSwitches;
use crate::throttle::{AuthThrottle, RateLimiter};
use crate::tls::TlsProvider;

#[derive(Clone)]
pub struct Context {
    pub directory: Directory,
    pub auth: Arc<dyn Authenticator>,
    pub hub: MailboxHub,
    pub jobs: JobQueue,
    pub gc: Gc,
    /// The platform blob store (the outbound queue's bodies live here).
    pub blobs: Arc<dyn BlobStore>,
    /// Every blob store: the platform store and the stores of the domains that have their own.
    pub stores: Arc<dyn BlobRouter>,
    pub cfg: Arc<StartupConfig>,
    pub tls: TlsProvider,
    pub events: Arc<dyn ServiceEvents>,
    pub rate: Arc<dyn RateLimiter>,
    pub quota: Arc<dyn QuotaPolicy>,
    /// The operator's protocol switches (read from the database, cached).
    pub protocols: ProtocolSwitches,
    /// The outbound queue as the admin web manages it and the delivery log the outbound worker writes.
    pub queue: Arc<dyn rusty_store::QueueRepo>,
    /// The audit log: written by the admin web, purged by the garbage collector.
    pub audit: Arc<dyn rusty_store::AuditRepo>,
    /// The platform administrator's searches of the mail (docs/design-admin-search.md): run by the IMAP service, read by the admin web.
    pub searches: Arc<dyn rusty_store::SearchRepo>,
    /// The book-keeping of automatic replies (vacation).
    pub rules: Arc<dyn rusty_store::RulesRepo>,
    /// The exports and imports of mailboxes asked for in the admin web.
    pub transfers: Arc<dyn rusty_store::TransferRepo>,
    /// Plans and what a mailbox holds (the IMAP `QUOTA` answer).
    pub plans: Arc<dyn rusty_store::PlanRepo>,
}

impl Context {
    /// Wire the facades over `repos` and install the default seam implementations
    /// (`TracingEvents`, the in-memory `AuthThrottle`, the `SizeCap` quota policy).
    pub async fn new(
        repos: &Repos,
        blobs: Arc<dyn BlobStore>,
        cfg: Arc<StartupConfig>,
        tls: TlsProvider,
    ) -> Result<Context> {
        // Stores of kind `fs` live next to the platform blob directory, in `<parent of blob_dir>/stores/<name>`; no other kind can be opened here.
        let stores_dir = cfg.blob_dir.parent().map_or_else(|| std::path::PathBuf::from("stores"), |p| p.join("stores"));
        let opener: Arc<dyn StoreOpener> = Arc::new(KindOpener::new().with_fs(Arc::new(FsStoreOpener::new(stores_dir))));
        Context::new_with_opener(repos, blobs, cfg, tls, opener).await
    }

    /// Like [`Context::new`], with the opener of the tenants' blob stores (the composition root adds the S3 opener to the filesystem one).
    pub async fn new_with_opener(
        repos: &Repos,
        blobs: Arc<dyn BlobStore>,
        cfg: Arc<StartupConfig>,
        tls: TlsProvider,
        opener: Arc<dyn StoreOpener>,
    ) -> Result<Context> {
        let directory = Directory::with_repo(repos.directory.clone());
        let auth: Arc<dyn Authenticator> = Arc::new(LocalAuthenticator::new(directory.clone()).await?);
        let limits = MailboxLimits {
            max_message_bytes: usize::try_from(cfg.max_message_bytes).unwrap_or(usize::MAX),
            quota_bytes: None,
        };
        let stores: Arc<dyn BlobRouter> = Arc::new(BlobStores::new(blobs.clone(), repos.blob_stores.clone(), opener));
        Ok(Context {
            auth,
            hub: MailboxHub::new(repos.mailbox.clone(), stores.clone(), limits).with_directory(directory.clone()),
            jobs: JobQueue::with_repo(repos.jobs.clone()),
            gc: Gc::new(repos.gc.clone()),
            directory,
            blobs,
            stores,
            tls,
            events: Arc::new(TracingEvents),
            rate: Arc::new(AuthThrottle::from_limits(&cfg.limits)),
            quota: Arc::new(SizeCap { max_message_bytes: cfg.max_message_bytes }),
            protocols: ProtocolSwitches::new(repos.settings.clone()),
            queue: repos.queue.clone(),
            audit: repos.audit.clone(),
            searches: repos.searches.clone(),
            rules: repos.rules.clone(),
            transfers: repos.transfers.clone(),
            plans: repos.plans.clone(),
            cfg,
        })
    }

    pub fn with_events(mut self, events: Arc<dyn ServiceEvents>) -> Context {
        self.events = events;
        self
    }

    pub fn with_rate(mut self, rate: Arc<dyn RateLimiter>) -> Context {
        self.rate = rate;
        self
    }

    pub fn with_quota(mut self, quota: Arc<dyn QuotaPolicy>) -> Context {
        self.quota = quota;
        self
    }
}
