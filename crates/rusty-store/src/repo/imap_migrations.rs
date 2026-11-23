//! `MigrationRepo`: the moves of a mailbox from another IMAP server (migration 0050, docs/design-imap-migration.md). The admin service creates, reads and cancels them; the service that holds the mailboxes
//! (the IMAP service) claims, reports on and finishes them; the garbage collector removes what has expired, with the sealed password.

use async_trait::async_trait;
use rusty_core::Result;

/// What the administrator asked for (the password is a sealed secret, not here).
#[derive(Debug, Clone)]
pub struct NewMigration {
    pub account: i64,
    pub requested_by: String,
    pub host: String,
    pub port: i32,
    /// `tls`, `starttls` or `plain`.
    pub security: String,
    pub remote_user: String,
}

/// A migration as stored.
#[derive(Debug, Clone)]
pub struct Migration {
    pub id: i64,
    pub account: i64,
    pub requested_by: String,
    pub host: String,
    pub port: i32,
    pub security: String,
    pub remote_user: String,
    /// `queued`, `running`, `done`, `failed` or `cancelled`.
    pub state: String,
    pub cancel_requested: bool,
    pub folder: Option<String>,
    pub folders_total: i32,
    pub folders_done: i32,
    pub messages: i64,
    pub skipped: i64,
    pub bytes: i64,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// What the worker reports between messages.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub folder: Option<String>,
    pub folders_total: i32,
    pub folders_done: i32,
    pub messages: i64,
    pub skipped: i64,
    pub bytes: i64,
}

/// The sealed secret's owner kind and purpose for the password of the other server.
pub const SECRET_OWNER: &str = "migration";
pub const SECRET_PURPOSE: &str = "password";

#[async_trait]
pub trait MigrationRepo: Send + Sync {
    /// Ask for a migration (`preparing`: the worker does not take it yet); returns its id. The caller seals the password under owner kind `migration` and this id, and then calls [`queue`](Self::queue).
    async fn create(&self, m: &NewMigration) -> Result<i64>;
    /// The password is sealed: the migration may be taken. `false` when it was not `preparing`.
    async fn queue(&self, id: i64) -> Result<bool>;
    async fn get(&self, id: i64) -> Result<Option<Migration>>;
    /// The newest of an account, at most `limit`.
    async fn list_for_account(&self, account: i64, limit: i64) -> Result<Vec<Migration>>;
    /// The worker takes the oldest queued migration (whoever asks first): it becomes `running`.
    async fn claim(&self) -> Result<Option<Migration>>;
    /// The worker's report: the counts so far. Returns `false` when the migration is no longer to go on (the administrator asked to stop, or it is not `running` any more).
    async fn progress(&self, id: i64, p: &Progress) -> Result<bool>;
    /// The worker is done: `done`, `failed` or `cancelled`, with the fixed word of why for a failure.
    async fn finish(&self, id: i64, state: &str, error: Option<&str>, p: &Progress) -> Result<()>;
    /// Stop it: a `queued` one is `cancelled` at once, a `running` one is asked to stop; `false` for any other.
    async fn request_cancel(&self, id: i64) -> Result<bool>;
    /// Remove the migrations that have expired; how many, at most `limit`.
    async fn purge_expired(&self, limit: i64) -> Result<u64>;
    /// Remove the sealed passwords of migrations that are gone (the admin service does this: no service that works on the mail may write a sealed secret); how many.
    async fn purge_orphan_secrets(&self) -> Result<u64>;
    /// A migration `running` with no sign of life from the worker for this many seconds is made `failed`.
    async fn fail_stuck(&self, older_than_secs: i64) -> Result<u64>;
}

pub(crate) mod sql {
    pub const CREATE: &str = "INSERT INTO imap_migrations (account_id, requested_by, host, port, security, remote_user, created_at, expires_at) \
        VALUES ($1, $2, $3, $4, $5, $6, rs_now(), rs_now() + 172800) RETURNING id";
    pub const QUEUE: &str = "UPDATE imap_migrations SET state = 'queued' WHERE id = $1 AND state = 'preparing'";
    pub const GET: &str = "SELECT id, account_id, requested_by, host, port, security, remote_user, state, cancel_requested, folder, folders_total, folders_done, messages, skipped, bytes, error, created_at, updated_at, finished_at \
        FROM imap_migrations WHERE id = $1";
    pub const LIST: &str = "SELECT id, account_id, requested_by, host, port, security, remote_user, state, cancel_requested, folder, folders_total, folders_done, messages, skipped, bytes, error, created_at, updated_at, finished_at \
        FROM imap_migrations WHERE account_id = $1 ORDER BY id DESC LIMIT $2";
    pub const CLAIM: &str = "UPDATE imap_migrations SET state = 'running', updated_at = rs_now() WHERE id = (SELECT id FROM imap_migrations WHERE state = 'queued' ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED) \
        RETURNING id, account_id, requested_by, host, port, security, remote_user, state, cancel_requested, folder, folders_total, folders_done, messages, skipped, bytes, error, created_at, updated_at, finished_at";
    pub const PROGRESS: &str = "UPDATE imap_migrations SET folder = $2, folders_total = $3, folders_done = $4, messages = $5, skipped = $6, bytes = $7, updated_at = rs_now() \
        WHERE id = $1 AND state = 'running' RETURNING cancel_requested";
    pub const FINISH: &str = "UPDATE imap_migrations SET state = $2, error = $3, folder = $4, folders_total = $5, folders_done = $6, messages = $7, skipped = $8, bytes = $9, updated_at = rs_now(), finished_at = rs_now() \
        WHERE id = $1 AND state = 'running'";
    pub const CANCEL_QUEUED: &str = "UPDATE imap_migrations SET state = 'cancelled', finished_at = rs_now() WHERE id = $1 AND state IN ('preparing', 'queued')";
    pub const CANCEL_RUNNING: &str = "UPDATE imap_migrations SET cancel_requested = true WHERE id = $1 AND state = 'running'";
    pub const PURGE: &str = "WITH gone AS (DELETE FROM imap_migrations WHERE id IN (SELECT id FROM imap_migrations WHERE expires_at < rs_now() ORDER BY id LIMIT $1) RETURNING id) \
        SELECT count(*) FROM gone";
    pub const PURGE_SECRETS: &str = "DELETE FROM sealed_secrets WHERE owner_kind = 'migration' AND NOT EXISTS (SELECT 1 FROM imap_migrations m WHERE m.id = sealed_secrets.owner_id)";
    pub const FAIL_STUCK: &str = "UPDATE imap_migrations SET state = 'failed', error = 'failed', finished_at = rs_now() WHERE state = 'running' AND COALESCE(updated_at, created_at) < rs_now() - $1";
}
