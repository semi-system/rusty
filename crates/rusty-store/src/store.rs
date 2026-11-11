//! `Store::open(StoreConfig)`: chooses the backend and hands out the repositories.
//!
//! `Repos` holds the directory, mailbox, jobs and GC repositories as `Arc<dyn Trait>`.

use std::sync::Arc;
use std::time::Duration;

use rusty_core::{Error, Result};

use crate::repo::sqlite::SqliteRepo;
use crate::repo::{DirectoryRepo, GcRepo, JobRepo, MailboxRepo};
use crate::{SqliteStore, DEFAULT_BUSY_TIMEOUT};

/// Which backend to open and how.
#[derive(Clone)]
pub enum StoreConfig {
    /// SQLite: `url` is a sqlx URL (`sqlite://path` or `sqlite::memory:`).
    Sqlite { url: String, busy_timeout: Duration },
    /// Postgres. `security` carries the TLS mode, CA, password and role policy; with the default
    /// value the URL decides (and a non-loopback host must still use `verify-full`).
    #[cfg(feature = "postgres")]
    Postgres {
        url: String,
        pool: crate::PgLimits,
        security: crate::PgSecurity,
    },
}

// Manual Debug: the Postgres URL carries the password, so it is never printed (same contract as
// `PgStore`). SQLite URLs are file paths and stay visible.
impl std::fmt::Debug for StoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreConfig::Sqlite { url, busy_timeout } => f
                .debug_struct("Sqlite")
                .field("url", url)
                .field("busy_timeout", busy_timeout)
                .finish(),
            #[cfg(feature = "postgres")]
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
    /// SQLite with the default busy timeout.
    pub fn sqlite(url: impl Into<String>) -> Self {
        StoreConfig::Sqlite {
            url: url.into(),
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
        }
    }

    /// Postgres with the default pool settings.
    #[cfg(feature = "postgres")]
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
}

/// An opened store: the repositories plus the backend handle that not-yet-converted facades
/// (`Mailbox`, `JobQueue`, GC) still take.
#[derive(Clone)]
pub struct Store {
    repos: Repos,
    sqlite: Option<SqliteStore>,
    #[cfg(feature = "postgres")]
    pg: Option<crate::PgStore>,
}

impl Store {
    /// Open the configured backend. SQLite migrates its own file; Postgres is NEVER migrated here
    /// (`rustyctl db migrate` does that): a schema that is not exactly this binary's is refused.
    pub async fn open(config: StoreConfig) -> Result<Store> {
        match config {
            StoreConfig::Sqlite { url, busy_timeout } => {
                if busy_timeout.is_zero() {
                    return Err(Error::Invalid("invalid SQLite busy timeout".into()));
                }
                let store = SqliteStore::open_with_busy_timeout(&url, busy_timeout).await?;
                let repo = Arc::new(SqliteRepo::new(store.clone()));
                Ok(Store {
                    repos: Repos {
                        directory: repo.clone(),
                        mailbox: repo.clone(),
                        jobs: repo.clone(),
                        gc: repo,
                    },
                    sqlite: Some(store),
                    #[cfg(feature = "postgres")]
                    pg: None,
                })
            }
            #[cfg(feature = "postgres")]
            StoreConfig::Postgres { url, pool, security } => {
                let store = crate::PgStore::connect_with(&url, pool, &security).await?;
                let repo = Arc::new(crate::repo::pg::PgRepo::new(store.clone()));
                Ok(Store {
                    repos: Repos {
                        directory: repo.clone(),
                        mailbox: repo.clone(),
                        jobs: repo.clone(),
                        gc: repo,
                    },
                    sqlite: None,
                    pg: Some(store),
                })
            }
        }
    }

    pub fn repos(&self) -> &Repos {
        &self.repos
    }

    /// Graceful shutdown of the backend (SQLite: WAL checkpointed, then connections closed;
    /// Postgres: pool closed). Call once after every service holding `Repos` has stopped.
    pub async fn close(&self) -> Result<()> {
        if let Some(s) = &self.sqlite {
            s.close().await?;
        }
        #[cfg(feature = "postgres")]
        if let Some(p) = &self.pg {
            p.close().await;
        }
        Ok(())
    }

    /// The SQLite handle, `None` on Postgres. For facades that are not converted yet.
    pub fn sqlite(&self) -> Option<&SqliteStore> {
        self.sqlite.as_ref()
    }

    #[cfg(feature = "postgres")]
    pub fn postgres(&self) -> Option<&crate::PgStore> {
        self.pg.as_ref()
    }
}
