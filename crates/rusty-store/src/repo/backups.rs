//! `BackupRepo`: what the deployment tool did about backups (migration 0048, docs/design-backup-status.md). The tool writes the rows; the services only read and purge.

use async_trait::async_trait;
use rusty_core::Result;

/// One backup, drill or restore as read back.
#[derive(Debug, Clone)]
pub struct BackupRun {
    pub id: i64,
    pub at: i64,
    /// `backup`, `verify` or `restore`.
    pub kind: String,
    pub ok: bool,
    pub bytes: Option<i64>,
    pub detail: String,
}

#[async_trait]
pub trait BackupRepo: Send + Sync {
    /// Newest first.
    async fn list(&self, limit: i64) -> Result<Vec<BackupRun>>;
    /// Remove up to `limit` rows older than `older_than` (seconds); returns how many went.
    async fn purge(&self, older_than: i64, limit: i64) -> Result<u64>;
}

pub(crate) mod sql {
    pub const LIST: &str = "SELECT id, at, kind, ok, bytes, detail FROM backup_runs ORDER BY id DESC LIMIT $1";
    pub const PURGE: &str = "DELETE FROM backup_runs WHERE id IN (SELECT id FROM backup_runs WHERE at < $1 ORDER BY id LIMIT $2)";
}
