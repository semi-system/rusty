//! `DavRepo`: the calendars and address books of an account and the objects in them (migration 0058, docs/design-caldav-carddav.md). The DAV service reads and writes through it; the collector drops old
//! tombstones. Every write of an object is one transaction that checks the quotas, writes the object and adds one to the revision of its collection, so two clients never leave a collection half changed.

use async_trait::async_trait;
use rusty_core::{AccountId, Result};

pub const MAX_COLLECTIONS_PER_ACCOUNT: i64 = 50;
pub const MAX_OBJECTS_PER_COLLECTION: i64 = 10_000;
pub const MAX_OBJECTS_PER_ACCOUNT: i64 = 100_000;
pub const MAX_OBJECT_BYTES: usize = 1024 * 1024;
/// How long a tombstone is kept: a client whose sync token is older does a full sync.
pub const TOMBSTONE_KEEP_SECS: i64 = 30 * 24 * 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub id: i64,
    /// `calendar` or `addressbook`.
    pub kind: String,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub color: String,
    /// The count of changes: the ctag, and the base of the sync token.
    pub revision: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub name: String,
    pub uid: String,
    pub etag: String,
    pub body: String,
    pub size: i64,
    pub component: String,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub recurring: bool,
    pub revision: i64,
    pub updated_at: i64,
}

/// What a client uploads, after the reader of the format accepted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewObject {
    pub name: String,
    pub uid: String,
    pub body: String,
    pub component: String,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub recurring: bool,
}

/// The condition of a write or a delete (`If-Match`, `If-None-Match: *`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    None,
    /// `If-None-Match: *`: the object must not be there.
    Absent,
    /// `If-Match`: the object must be there with this entity tag.
    Etag(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutResult {
    Created { etag: String, revision: i64 },
    Updated { etag: String, revision: i64 },
    /// The precondition did not hold.
    PreconditionFailed,
    /// Another object of the collection has this UID; its name.
    UidConflict(String),
    /// The collection or the account is full (`objects` or `account objects`).
    Full(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteResult {
    Deleted { revision: i64 },
    NotFound,
    PreconditionFailed,
}

/// What changed in a collection since a revision: the sync report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changes {
    /// The revision to hand back as the new token.
    pub revision: i64,
    /// Objects (name and etag) written since, oldest first.
    pub changed: Vec<(String, String)>,
    /// Names deleted since.
    pub deleted: Vec<String>,
    /// There was more than `limit`: the client asks again from `revision`.
    pub truncated: bool,
}

#[async_trait]
pub trait DavRepo: Send + Sync {
    /// The collections of one kind of an account, by name.
    async fn collections(&self, account: AccountId, kind: &str) -> Result<Vec<Collection>>;
    async fn collection(&self, account: AccountId, kind: &str, name: &str) -> Result<Option<Collection>>;
    /// A new account has a calendar and an address book named `default`: made when they are not there (and never again once the person removed them: only when the account has no collection at all).
    async fn ensure_defaults(&self, account: AccountId) -> Result<()>;
    /// `Error::Conflict` when the name is taken, `Error::Quota` past [`MAX_COLLECTIONS_PER_ACCOUNT`], `Error::Invalid` for a name or a field the table refuses.
    async fn create_collection(&self, account: AccountId, kind: &str, name: &str, display_name: &str, description: &str) -> Result<Collection>;
    /// Change the fields given; whether the collection exists.
    async fn update_collection(&self, account: AccountId, kind: &str, name: &str, display_name: Option<&str>, description: Option<&str>, color: Option<&str>) -> Result<bool>;
    /// Remove a collection with its objects; whether there was one.
    async fn delete_collection(&self, account: AccountId, kind: &str, name: &str) -> Result<bool>;

    /// One object.
    async fn object(&self, collection: i64, name: &str) -> Result<Option<Object>>;
    /// Every object (at most `limit`), by name.
    async fn objects(&self, collection: i64, limit: i64) -> Result<Vec<Object>>;
    /// The named objects that exist.
    async fn objects_named(&self, collection: i64, names: &[String]) -> Result<Vec<Object>>;
    /// The objects of a component kind (`None`: any) whose bounds meet `[from, to)` (a bound that is `None` is open): an object with no end meets every range that starts after its start; one with no start is not
    /// bounded and always meets. At most `limit`.
    async fn objects_in_range(&self, collection: i64, component: Option<&str>, from: Option<i64>, to: Option<i64>, limit: i64) -> Result<Vec<Object>>;
    async fn put(&self, collection: i64, new: &NewObject, pre: &Precondition) -> Result<PutResult>;
    async fn delete(&self, collection: i64, name: &str, pre: &Precondition) -> Result<DeleteResult>;
    /// What changed since `since` (the revision of a token); `None` when tombstones the client needs were collected (it does a full sync).
    async fn changes_since(&self, collection: i64, since: i64, limit: i64) -> Result<Option<Changes>>;
    /// The collector: drop the tombstones older than [`TOMBSTONE_KEEP_SECS`] and raise the floor of their collections; how many went.
    async fn purge_tombstones(&self) -> Result<u64>;
}

macro_rules! object_select {
    ($tail:literal) => {
        concat!("SELECT name, uid, etag, body, size, component, starts_at, ends_at, recurring, revision, updated_at FROM dav_objects ", $tail)
    };
}

pub(crate) mod sql {
    pub const COLLECTIONS: &str = "SELECT id, kind, name, display_name, description, color, revision, created_at, updated_at FROM dav_collections WHERE account_id = $1 AND kind = $2 ORDER BY name";
    pub const COLLECTION: &str = "SELECT id, kind, name, display_name, description, color, revision, created_at, updated_at FROM dav_collections WHERE account_id = $1 AND kind = $2 AND name = $3";
    pub const ANY_COLLECTION: &str = "SELECT EXISTS (SELECT 1 FROM dav_collections WHERE account_id = $1)";
    pub const COUNT_COLLECTIONS: &str = "SELECT count(*) FROM dav_collections WHERE account_id = $1";
    pub const CREATE_COLLECTION: &str = "INSERT INTO dav_collections (account_id, kind, name, display_name, description, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, rs_now(), rs_now()) \
        ON CONFLICT (account_id, kind, name) DO NOTHING RETURNING id, kind, name, display_name, description, color, revision, created_at, updated_at";
    pub const UPDATE_COLLECTION: &str = "UPDATE dav_collections SET display_name = COALESCE($4, display_name), description = COALESCE($5, description), color = COALESCE($6, color), updated_at = rs_now() \
        WHERE account_id = $1 AND kind = $2 AND name = $3";
    pub const DELETE_COLLECTION: &str = "DELETE FROM dav_collections WHERE account_id = $1 AND kind = $2 AND name = $3";

    pub const OBJECT: &str = object_select!("WHERE collection_id = $1 AND name = $2");
    pub const OBJECTS: &str = object_select!("WHERE collection_id = $1 ORDER BY name LIMIT $2");
    pub const OBJECTS_NAMED: &str = object_select!("WHERE collection_id = $1 AND name = ANY($2) ORDER BY name");
    pub const OBJECTS_RANGE: &str = object_select!("WHERE collection_id = $1 AND ($2::text IS NULL OR component = $2) \
        AND ($4::bigint IS NULL OR starts_at IS NULL OR starts_at < $4) AND ($3::bigint IS NULL OR ends_at IS NULL OR ends_at > $3 OR (starts_at = ends_at AND starts_at >= $3)) ORDER BY name LIMIT $5");

    /// The collection row, locked for the change: serialises the writers of one collection.
    pub const LOCK: &str = "SELECT revision FROM dav_collections WHERE id = $1 FOR UPDATE";
    pub const BUMP: &str = "UPDATE dav_collections SET revision = revision + 1, updated_at = rs_now() WHERE id = $1 RETURNING revision";
    pub const ACCOUNT_OF: &str = "SELECT account_id FROM dav_collections WHERE id = $1";
    pub const COUNT_IN_COLLECTION: &str = "SELECT count(*) FROM dav_objects WHERE collection_id = $1";
    pub const COUNT_IN_ACCOUNT: &str = "SELECT count(*) FROM dav_objects o JOIN dav_collections c ON c.id = o.collection_id WHERE c.account_id = $1";
    pub const UID_OWNER: &str = "SELECT name FROM dav_objects WHERE collection_id = $1 AND uid = $2 AND name <> $3";
    pub const ETAG_OF: &str = "SELECT etag FROM dav_objects WHERE collection_id = $1 AND name = $2";
    pub const UPSERT: &str = "INSERT INTO dav_objects (collection_id, name, uid, etag, body, size, component, starts_at, ends_at, recurring, revision, updated_at) \
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, rs_now()) ON CONFLICT (collection_id, name) DO UPDATE SET uid = EXCLUDED.uid, etag = EXCLUDED.etag, body = EXCLUDED.body, size = EXCLUDED.size, \
        component = EXCLUDED.component, starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at, recurring = EXCLUDED.recurring, revision = EXCLUDED.revision, updated_at = EXCLUDED.updated_at";
    pub const DROP_TOMBSTONE: &str = "DELETE FROM dav_tombstones WHERE collection_id = $1 AND name = $2";
    pub const DELETE_OBJECT: &str = "DELETE FROM dav_objects WHERE collection_id = $1 AND name = $2";
    pub const TOMBSTONE: &str = "INSERT INTO dav_tombstones (collection_id, name, revision, deleted_at) VALUES ($1, $2, $3, rs_now()) ON CONFLICT (collection_id, name) DO UPDATE SET revision = EXCLUDED.revision, deleted_at = EXCLUDED.deleted_at";
    pub const REVISION_AND_FLOOR: &str = "SELECT revision, tombstone_floor FROM dav_collections WHERE id = $1";
    pub const CHANGED: &str = "SELECT name, etag, revision FROM dav_objects WHERE collection_id = $1 AND revision > $2 ORDER BY revision, name LIMIT $3";
    pub const DELETED: &str = "SELECT name FROM dav_tombstones WHERE collection_id = $1 AND revision > $2 AND revision <= $3 ORDER BY revision, name";
    pub const PURGE: &str = "WITH gone AS (DELETE FROM dav_tombstones WHERE deleted_at < rs_now() - $1 RETURNING collection_id, revision), \
        floors AS (SELECT collection_id, max(revision) AS r FROM gone GROUP BY collection_id), \
        raised AS (UPDATE dav_collections c SET tombstone_floor = GREATEST(c.tombstone_floor, f.r) FROM floors f WHERE c.id = f.collection_id RETURNING 1) \
        SELECT count(*) FROM gone";
}
