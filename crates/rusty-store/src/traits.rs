//! Backend-neutral metadata store traits (spec §5).
//!
//! Deliberately thin: `Mailbox` talks to `SqliteStore` directly until a second backend
//! (Postgres) exists and shows which operations belong on `Tx`.

use async_trait::async_trait;
use rusty_core::Result;
use sqlx::{Sqlite, SqliteConnection, Transaction};

use crate::{map_db_error, SqliteStore};

/// A write transaction. Dropping it without `commit` rolls it back.
#[async_trait]
pub trait Tx: Send {
    async fn commit(self: Box<Self>) -> Result<()>;
    async fn rollback(self: Box<Self>) -> Result<()>;
}

/// A metadata backend: every write goes through a `Tx`.
#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn tx(&self) -> Result<Box<dyn Tx>>;
}

/// SQLite write transaction (`BEGIN IMMEDIATE`).
pub struct SqliteTx(Transaction<'static, Sqlite>);

impl SqliteTx {
    /// Begin a write transaction (`BEGIN IMMEDIATE`, see [`SqliteStore::begin_write`]).
    ///
    /// Internal: public only for callers that fence job steps and for tests; it will be removed
    /// once the store service owns all database access.
    pub async fn begin(store: &SqliteStore) -> Result<SqliteTx> {
        Ok(SqliteTx(store.begin_write().await?))
    }

    pub fn conn(&mut self) -> &mut SqliteConnection {
        &mut self.0
    }

    /// Commit. (An inherent twin of [`Tx::commit`] for callers that hold a plain `SqliteTx`.)
    ///
    /// Internal: see [`SqliteTx::begin`].
    pub async fn commit(self) -> Result<()> {
        self.0.commit().await.map_err(map_db_error)
    }

    /// Roll back. Dropping the transaction has the same effect.
    ///
    /// Internal: see [`SqliteTx::begin`].
    pub async fn rollback(self) -> Result<()> {
        self.0.rollback().await.map_err(map_db_error)
    }
}

#[async_trait]
impl Tx for SqliteTx {
    async fn commit(self: Box<Self>) -> Result<()> {
        self.0.commit().await.map_err(map_db_error)
    }

    async fn rollback(self: Box<Self>) -> Result<()> {
        self.0.rollback().await.map_err(map_db_error)
    }
}

#[async_trait]
impl MetadataStore for SqliteStore {
    async fn tx(&self) -> Result<Box<dyn Tx>> {
        Ok(Box::new(SqliteTx(self.begin_write().await?)))
    }
}
