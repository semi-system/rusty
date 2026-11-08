//! `Context`: everything a service is built from. The composition root (`rusty`) is the only
//! place that opens a `Store`; it passes the repositories here. Services hold trait objects and
//! facades, so a `store` service or another backend needs no service change.

use std::sync::Arc;

use rusty_core::config::StartupConfig;
use rusty_core::Result;
use rusty_directory::{Authenticator, Directory, LocalAuthenticator};
use rusty_store::{BlobStore, Gc, JobQueue, MailboxLimits, Repos};

use crate::events::{ServiceEvents, TracingEvents};
use crate::hub::MailboxHub;
use crate::quota::{QuotaPolicy, SizeCap};
use crate::throttle::{AuthThrottle, RateLimiter};
use crate::tls::TlsProvider;

#[derive(Clone)]
pub struct Context {
    pub directory: Directory,
    pub auth: Arc<dyn Authenticator>,
    pub hub: MailboxHub,
    pub jobs: JobQueue,
    pub gc: Gc,
    pub blobs: Arc<dyn BlobStore>,
    pub cfg: Arc<StartupConfig>,
    pub tls: TlsProvider,
    pub events: Arc<dyn ServiceEvents>,
    pub rate: Arc<dyn RateLimiter>,
    pub quota: Arc<dyn QuotaPolicy>,
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
        let directory = Directory::with_repo(repos.directory.clone());
        let auth: Arc<dyn Authenticator> = Arc::new(LocalAuthenticator::new(directory.clone()).await?);
        let limits = MailboxLimits {
            max_message_bytes: usize::try_from(cfg.max_message_bytes).unwrap_or(usize::MAX),
        };
        Ok(Context {
            auth,
            hub: MailboxHub::new(repos.mailbox.clone(), blobs.clone(), limits),
            jobs: JobQueue::with_repo(repos.jobs.clone()),
            gc: Gc::new(repos.gc.clone()),
            directory,
            blobs,
            tls,
            events: Arc::new(TracingEvents),
            rate: Arc::new(AuthThrottle::from_limits(&cfg.limits)),
            quota: Arc::new(SizeCap { max_message_bytes: cfg.max_message_bytes }),
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
