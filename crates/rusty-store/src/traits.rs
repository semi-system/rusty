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
    pub fn conn(&mut self) -> &mut SqliteConnection {
        &mut self.0
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
