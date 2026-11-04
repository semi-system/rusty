//! SQLite implementation of the repositories (directory: `directory.rs`, mailbox:
//! `mailbox.rs`).
//!
//! Locking: `begin_locked` = `BEGIN IMMEDIATE` (global write lock) then touch the mailbox row.
//! Counters are taken with one `UPDATE .. RETURNING` each.

mod directory;
mod gc;
mod jobs;
mod mailbox;

use rusty_core::{MailboxId, Result};
use sqlx::{Sqlite, Transaction};

use super::not_found_mailbox;
use crate::{map_db_error, SqliteStore};

#[derive(Clone, Debug)]
pub struct SqliteRepo {
    store: SqliteStore,
}

impl SqliteRepo {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    /// `BEGIN IMMEDIATE`, then read the mailbox row first (`NotFound` if it is gone). With
    /// SQLite the write lock is already global; the fixed order is the lock-ordering rule that
    /// the row-locking backend relies on.
    async fn begin_locked(&self, mb: MailboxId) -> Result<Transaction<'static, Sqlite>> {
        let mut tx = self.store.begin_write().await?;
        sqlx::query_scalar::<_, i64>("SELECT id FROM mailboxes WHERE id = ?1")
            .bind(mb.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_db_error)?
            .ok_or_else(not_found_mailbox)?;
        Ok(tx)
    }
}
