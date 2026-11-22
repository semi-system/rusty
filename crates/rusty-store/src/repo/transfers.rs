//! `TransferRepo`: the exports and imports of a mailbox asked for in the admin web (migration 0046, docs/design-mailbox-transfers.md).

use async_trait::async_trait;
use rusty_core::Result;

/// One transfer as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub id: i64,
    /// `export` or `import`.
    pub kind: String,
    pub account: i64,
    pub requested_by: String,
    /// `uploading`, `queued`, `running`, `done` or `failed`.
    pub state: String,
    pub folder: Option<String>,
    /// Bytes of the file (an import's, as far as it has arrived; an export's, when it is done).
    pub size: i64,
    pub messages: i64,
    pub skipped: i64,
    pub error: Option<String>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    pub expires_at: i64,
}

/// The most bytes one transfer may hold: an import is uploaded through small requests and an export is kept in the database for a day.
pub const MAX_TRANSFER_BYTES: i64 = 256 * 1024 * 1024;

#[async_trait]
pub trait TransferRepo: Send + Sync {
    /// Ask for a transfer: an export is `queued` at once, an import is `uploading` until its file has arrived. Returns its id.
    async fn create(&self, kind: &str, account: i64, requested_by: &str, folder: Option<&str>) -> Result<i64>;
    /// Add a piece of the file (an upload, or the worker's output). `false`: the transfer is not in a state that takes pieces (`uploading` or `running`) or would pass `max_bytes`.
    async fn add_chunk(&self, id: i64, seq: i32, data: &[u8], max_bytes: i64) -> Result<bool>;
    /// One piece of the file (`None` past the last).
    async fn chunk(&self, id: i64, seq: i32) -> Result<Option<Vec<u8>>>;
    /// An import whose file has arrived is queued for the worker; `false` when it was not `uploading` or has no piece.
    async fn queue_import(&self, id: i64) -> Result<bool>;
    /// The worker takes the oldest queued transfer (one at a time, whoever asks first): it becomes `running`.
    async fn claim(&self) -> Result<Option<Transfer>>;
    /// The worker is done: `state` is `done` or `failed`, with the counts and, for a failure, the fixed word of why.
    async fn finish(&self, id: i64, state: &str, size: i64, messages: i64, skipped: i64, error: Option<&str>) -> Result<()>;
    async fn get(&self, id: i64) -> Result<Option<Transfer>>;
    /// The newest transfers of an account, at most `limit`.
    async fn list_for_account(&self, account: i64, limit: i64) -> Result<Vec<Transfer>>;
    /// Remove the transfers that have expired (their pieces go with them); how many, at most `limit`.
    async fn purge_expired(&self, limit: i64) -> Result<u64>;
    /// A transfer left `running` by a worker that stopped is made `failed` after this many seconds without finishing.
    async fn fail_stuck(&self, older_than_secs: i64) -> Result<u64>;
}

pub(crate) mod sql {
    pub const CREATE: &str = "INSERT INTO mailbox_transfers (kind, account_id, requested_by, state, folder, created_at, expires_at) \
        VALUES ($1, $2, $3, CASE WHEN $1 = 'export' THEN 'queued' ELSE 'uploading' END, $4, rs_now(), rs_now() + 86400) RETURNING id";
    /// One statement: the size is raised only while the transfer takes pieces and stays within the limit, and then the piece is stored.
    pub const ADD_CHUNK: &str = "WITH ok AS (UPDATE mailbox_transfers SET size = size + $4 WHERE id = $1 AND state IN ('uploading', 'running') AND size + $4 <= $5 RETURNING id) \
        INSERT INTO mailbox_transfer_chunks (transfer_id, seq, data) SELECT id, $2, $3 FROM ok";
    pub const CHUNK: &str = "SELECT data FROM mailbox_transfer_chunks WHERE transfer_id = $1 AND seq = $2";
    pub const QUEUE_IMPORT: &str = "UPDATE mailbox_transfers SET state = 'queued' WHERE id = $1 AND kind = 'import' AND state = 'uploading' AND EXISTS (SELECT 1 FROM mailbox_transfer_chunks WHERE transfer_id = $1)";
    pub const CLAIM: &str = "UPDATE mailbox_transfers SET state = 'running' WHERE id = (SELECT id FROM mailbox_transfers WHERE state = 'queued' ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED) \
        RETURNING id, kind, account_id, requested_by, state, folder, size, messages, skipped, error, created_at, finished_at, expires_at";
    pub const FINISH: &str = "UPDATE mailbox_transfers SET state = $2, size = $3, messages = $4, skipped = $5, error = $6, finished_at = rs_now() WHERE id = $1 AND state = 'running'";
    pub const GET: &str = "SELECT id, kind, account_id, requested_by, state, folder, size, messages, skipped, error, created_at, finished_at, expires_at FROM mailbox_transfers WHERE id = $1";
    pub const LIST: &str = "SELECT id, kind, account_id, requested_by, state, folder, size, messages, skipped, error, created_at, finished_at, expires_at FROM mailbox_transfers WHERE account_id = $1 ORDER BY id DESC LIMIT $2";
    pub const PURGE: &str = "DELETE FROM mailbox_transfers WHERE id IN (SELECT id FROM mailbox_transfers WHERE expires_at < rs_now() ORDER BY id LIMIT $1)";
    pub const FAIL_STUCK: &str = "UPDATE mailbox_transfers SET state = 'failed', error = 'failed', finished_at = rs_now() WHERE state = 'running' AND created_at < rs_now() - $1";
}
