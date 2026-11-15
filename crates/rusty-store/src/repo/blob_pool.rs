//! `BlobPoolRepo`: the backends of a pool store and where each object is (migration 0024, docs/design-s3-per-tenant.md, section 12).

use async_trait::async_trait;
use rusty_core::{BlobHash, Result};

use super::StoreId;

/// The most backends one pool may have.
pub const MAX_BACKENDS: usize = 8;

/// The identity of a backend of a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BackendId(pub i64);

/// What a backend may be used for. `Active`: reads and writes; `ReadOnly`: reads only; `Draining`: reads, and the repair job moves its objects away; `Disabled`: not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendState {
    Active,
    ReadOnly,
    Draining,
    Disabled,
}

impl BackendState {
    pub fn name(self) -> &'static str {
        match self {
            BackendState::Active => "active",
            BackendState::ReadOnly => "readonly",
            BackendState::Draining => "draining",
            BackendState::Disabled => "disabled",
        }
    }

    pub fn parse(s: &str) -> Option<BackendState> {
        Some(match s {
            "active" => BackendState::Active,
            "readonly" => BackendState::ReadOnly,
            "draining" => BackendState::Draining,
            "disabled" => BackendState::Disabled,
            _ => return None,
        })
    }

    /// New objects may be written here.
    pub fn writable(self) -> bool {
        self == BackendState::Active
    }

    /// Objects may be read from here.
    pub fn readable(self) -> bool {
        self != BackendState::Disabled
    }
}

/// One backend row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRow {
    pub id: BackendId,
    pub store: StoreId,
    pub name: String,
    /// `fs` or `s3`.
    pub kind: String,
    /// Non-secret settings as JSON text.
    pub config: String,
    pub weight: u32,
    pub state: BackendState,
    pub check_status: String,
    pub checked_at: Option<i64>,
}

/// Where one object is: the backend, and whether the blob is already gone and only the deletion is outstanding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub backend: BackendId,
    pub pending_delete: bool,
}

#[async_trait]
pub trait BlobPoolRepo: Send + Sync {
    /// The backends of a store, in the order they were added.
    async fn backends(&self, store: StoreId) -> Result<Vec<BackendRow>>;
    async fn backend(&self, id: BackendId) -> Result<Option<BackendRow>>;
    /// Add a backend to a store of kind `pool`: `Conflict` for a name already used in the store or when the store has [`MAX_BACKENDS`], `NotFound` for a store that is not a pool.
    async fn add_backend(&self, store: StoreId, name: &str, kind: &str, config: &str, weight: u32) -> Result<BackendId>;
    /// Replace the settings of a backend; `Conflict` while the backend holds an object (they would be left where the old settings point). The check result is reset.
    async fn update_backend_config(&self, id: BackendId, config: &str) -> Result<()>;
    async fn set_backend_state(&self, id: BackendId, state: BackendState) -> Result<()>;
    async fn set_backend_weight(&self, id: BackendId, weight: u32) -> Result<()>;
    /// Record the result of a check of a backend (a fixed category), at `now` or the database clock.
    async fn record_backend_check(&self, id: BackendId, status: &str, now: Option<i64>) -> Result<()>;
    /// Remove a backend that holds no object; `Conflict` otherwise (drain it first).
    async fn remove_backend(&self, id: BackendId) -> Result<()>;

    /// Record that these backends hold the object (idempotent; an outstanding deletion of the same object there is cancelled).
    async fn add_locations(&self, store: StoreId, hash: &BlobHash, backends: &[BackendId]) -> Result<()>;
    /// Every location of an object.
    async fn locations(&self, store: StoreId, hash: &BlobHash) -> Result<Vec<Location>>;
    /// The blob is gone: every location of it becomes an outstanding deletion.
    async fn mark_all_pending_delete(&self, store: StoreId, hash: &BlobHash) -> Result<()>;
    /// The object was deleted (or found missing) on this backend: forget the location.
    async fn remove_location(&self, store: StoreId, hash: &BlobHash, backend: BackendId) -> Result<()>;
    /// Outstanding deletions of a store, at most `limit`.
    async fn pending_deletes(&self, store: StoreId, limit: u32) -> Result<Vec<(BlobHash, BackendId)>>;
    /// Blobs of the store (that still have a row) with fewer than `copies` locations on backends other than the draining and disabled ones, after `after` in hash order.
    async fn under_replicated(&self, store: StoreId, copies: u32, after: Option<&BlobHash>, limit: u32) -> Result<Vec<BlobHash>>;
    /// Objects held by a backend (state `ok`), after `after` in hash order, at most `limit`: what a draining backend still has to give away.
    async fn held_by(&self, backend: BackendId, after: Option<&BlobHash>, limit: u32) -> Result<Vec<BlobHash>>;
    /// How many objects a backend holds (any state).
    async fn count_held(&self, backend: BackendId) -> Result<i64>;
}

pub(crate) mod sql {
    pub const BACKENDS: &str = "SELECT id, store_id, name, kind, CAST(config AS TEXT), weight, state, check_status, checked_at FROM blob_backends WHERE store_id = $1 ORDER BY id";
    pub const BACKEND: &str = "SELECT id, store_id, name, kind, CAST(config AS TEXT), weight, state, check_status, checked_at FROM blob_backends WHERE id = $1";
    pub const STORE_LOCK: &str = "SELECT kind FROM blob_stores WHERE id = $1 FOR UPDATE";
    pub const COUNT_BACKENDS: &str = "SELECT count(*) FROM blob_backends WHERE store_id = $1";
    pub const ADD_BACKEND: &str = "INSERT INTO blob_backends (store_id, name, kind, config, weight, created_at) VALUES ($1, $2, $3, CAST($4 AS JSONB), $5, {NOW}) RETURNING id";
    pub const BACKEND_LOCK: &str = "SELECT id FROM blob_backends WHERE id = $1 FOR UPDATE";
    pub const COUNT_HELD: &str = "SELECT count(*) FROM blob_locations WHERE backend_id = $1";
    pub const UPDATE_BACKEND_CONFIG: &str = "UPDATE blob_backends SET config = CAST($2 AS JSONB), check_status = 'unchecked', checked_at = NULL WHERE id = $1";
    pub const SET_BACKEND_STATE: &str = "UPDATE blob_backends SET state = $2 WHERE id = $1";
    pub const SET_BACKEND_WEIGHT: &str = "UPDATE blob_backends SET weight = $2 WHERE id = $1";
    pub const RECORD_BACKEND_CHECK: &str =
        "UPDATE blob_backends SET check_status = $2, checked_at = CASE WHEN $2 = 'unchecked' THEN NULL ELSE COALESCE($3, {NOW}) END WHERE id = $1";
    pub const REMOVE_BACKEND: &str = "DELETE FROM blob_backends WHERE id = $1";
    pub const ADD_LOCATIONS: &str =
        "INSERT INTO blob_locations (store_id, hash, backend_id, state) SELECT $1, $2, x, 'ok' FROM unnest($3::bigint[]) AS x ON CONFLICT (store_id, hash, backend_id) DO UPDATE SET state = 'ok'";
    pub const LOCATIONS: &str = "SELECT backend_id, state FROM blob_locations WHERE store_id = $1 AND hash = $2 ORDER BY backend_id";
    pub const MARK_PENDING: &str = "UPDATE blob_locations SET state = 'pending_delete' WHERE store_id = $1 AND hash = $2";
    pub const REMOVE_LOCATION: &str = "DELETE FROM blob_locations WHERE store_id = $1 AND hash = $2 AND backend_id = $3";
    pub const PENDING: &str = "SELECT hash, backend_id FROM blob_locations WHERE store_id = $1 AND state = 'pending_delete' ORDER BY hash, backend_id LIMIT $2";
    pub const UNDER_REPLICATED: &str = "SELECT l.hash FROM blob_locations l JOIN blob_backends b ON b.id = l.backend_id JOIN blobs x ON x.store_id = l.store_id AND x.hash = l.hash \
         WHERE l.store_id = $1 AND l.state = 'ok' AND ($3::bytea IS NULL OR l.hash > $3) \
         GROUP BY l.hash HAVING count(*) FILTER (WHERE b.state IN ('active', 'readonly')) < $2 ORDER BY l.hash LIMIT $4";
    pub const HELD_BY: &str = "SELECT hash FROM blob_locations WHERE backend_id = $1 AND state = 'ok' AND ($2::bytea IS NULL OR hash > $2) ORDER BY hash LIMIT $3";
}
