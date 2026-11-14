//! `Store::open(StoreConfig)`: connects to Postgres and hands out the repositories.
//!
//! `Repos` holds the directory, mailbox, jobs, GC and DNS repositories as `Arc<dyn Trait>`.

use std::sync::Arc;

use rusty_core::Result;

use crate::repo::{DirectoryRepo, DnsRepo, GcRepo, JobRepo, MailboxRepo};

/// How to reach the database. `security` carries the TLS mode, CA, password and role policy; with
/// the default value the URL decides (and a non-loopback host must still use `verify-full`).
#[derive(Clone)]
pub enum StoreConfig {
    Postgres {
        url: String,
        pool: crate::PgLimits,
        security: crate::PgSecurity,
    },
}

// Manual Debug: the Postgres URL carries the password, so it is never printed (same contract as
// `PgStore`).
impl std::fmt::Debug for StoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreConfig::Postgres { pool, security, .. } => f
                .debug_struct("Postgres")
                .field("url", &"<redacted>")
                .field("pool", pool)
                .field("security", security)
                .finish(),
        }
    }
}

impl StoreConfig {
    /// Postgres with the default pool settings.
    pub fn postgres(url: impl Into<String>) -> Self {
        StoreConfig::Postgres {
            url: url.into(),
            pool: crate::PgLimits::default(),
            security: crate::PgSecurity::default(),
        }
    }
}

/// The repositories of one opened store.
#[derive(Clone)]
pub struct Repos {
    pub directory: Arc<dyn DirectoryRepo>,
    pub mailbox: Arc<dyn MailboxRepo>,
    pub jobs: Arc<dyn JobRepo>,
    pub gc: Arc<dyn GcRepo>,
    pub dns: Arc<dyn DnsRepo>,
    pub sealed: Arc<dyn crate::repo::SealedRepo>,
}

/// An opened store: the repositories plus the Postgres handle.
#[derive(Clone)]
pub struct Store {
    repos: Repos,
    pg: crate::PgStore,
}

impl Store {
    /// Connect to Postgres. The schema is NEVER migrated here (`rustyctl db migrate` does that): a
    /// schema that is not exactly this binary's is refused.
    pub async fn open(config: StoreConfig) -> Result<Store> {
        match config {
            StoreConfig::Postgres { url, pool, security } => {
                let store = crate::PgStore::connect_with(&url, pool, &security).await?;
                let repo = Arc::new(crate::repo::pg::PgRepo::new(store.clone()));
                Ok(Store {
                    repos: Repos {
                        directory: repo.clone(),
                        mailbox: repo.clone(),
                        jobs: repo.clone(),
                        gc: repo.clone(),
                        dns: repo.clone(),
                        sealed: repo,
                    },
                    pg: store,
                })
            }
        }
    }

    pub fn repos(&self) -> &Repos {
        &self.repos
    }

    /// The feed of changes made by other processes: run it as a service of the process and give it a sink.
    pub fn change_feed(&self) -> Arc<dyn crate::ChangeFeed> {
        Arc::new(self.pg.change_feed())
    }

    /// Graceful shutdown: the pool is closed. Call once after every service holding `Repos` has stopped.
    pub async fn close(&self) -> Result<()> {
        self.pg.close().await;
        Ok(())
    }

    /// The Postgres handle.
    pub fn postgres(&self) -> &crate::PgStore {
        &self.pg
    }
}
