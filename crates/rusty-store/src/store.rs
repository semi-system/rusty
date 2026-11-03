//! `Store::open(StoreConfig)`: chooses the backend and hands out the repositories.
//!
//! `Repos` holds the directory and mailbox repositories as `Arc<dyn Trait>`. The jobs and GC
//! repositories join in PG5; until then `JobQueue` and the GC functions take the SQLite store
//! directly (`Store::sqlite()`), and `Repos::mailbox` is the PG2 spike repository that PG4
//! replaces.

use std::sync::Arc;
use std::time::Duration;

use rusty_core::{Error, Result};

use crate::repo::sqlite::SqliteRepo;
use crate::repo::{DirectoryRepo, MailboxRepo};
use crate::{SqliteStore, DEFAULT_BUSY_TIMEOUT};

/// Which backend to open and how.
#[derive(Clone)]
pub enum StoreConfig {
    /// SQLite: `url` is a sqlx URL (`sqlite://path` or `sqlite::memory:`).
    Sqlite { url: String, busy_timeout: Duration },
    /// Postgres. TLS mode and certificates come from the URL (`sslmode=`); a hardened
    /// `verify-full` policy is PG8.
    #[cfg(feature = "postgres")]
    Postgres {
        url: String,
        pool: crate::PgPoolConfig,
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
            StoreConfig::Postgres { pool, .. } => f
                .debug_struct("Postgres")
                .field("url", &"<redacted>")
                .field("pool", pool)
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
            pool: crate::PgPoolConfig::default(),
        }
    }
}

/// The repositories of one opened store.
#[derive(Clone)]
pub struct Repos {
    pub directory: Arc<dyn DirectoryRepo>,
    pub mailbox: Arc<dyn MailboxRepo>,
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
    /// Open (and migrate) the configured backend.
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
                        mailbox: repo,
                    },
                    sqlite: Some(store),
                    #[cfg(feature = "postgres")]
                    pg: None,
                })
            }
            #[cfg(feature = "postgres")]
            StoreConfig::Postgres { url, pool } => {
                let store = crate::PgStore::connect_with_config(&url, pool).await?;
                let repo = Arc::new(crate::repo::pg::PgRepo::new(store.clone()));
                Ok(Store {
                    repos: Repos {
                        directory: repo.clone(),
                        mailbox: repo,
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

    /// The SQLite handle, `None` on Postgres. For facades that are not converted yet.
    pub fn sqlite(&self) -> Option<&SqliteStore> {
        self.sqlite.as_ref()
    }

    #[cfg(feature = "postgres")]
    pub fn postgres(&self) -> Option<&crate::PgStore> {
        self.pg.as_ref()
    }
}
