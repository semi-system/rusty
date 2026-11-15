//! Which blob store holds a body: the seam between the code that reads and writes message bodies and the several stores a deployment may have
//! (docs/design-s3-per-tenant.md). Everything that used to hold one `Arc<dyn BlobStore>` can hold a [`BlobRouter`]; a message row names its store, the router returns it.
//!
//! * [`SingleStore`]: one store, the platform store (what a deployment without tenant stores is, and what the tests use). Any other store id is `NotFound`.
//! * [`BlobStores`]: the platform store plus the stores of the `blob_stores` table, opened on first use by a [`StoreOpener`] and kept.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use rusty_core::{Error, Result};

use crate::pool::{PoolPolicy, PoolStore};
use crate::repo::{BackendRow, BackendState, BlobPoolRepo, BlobStoreRepo, BlobStoreRow, StoreId, PLATFORM_STORE};
use crate::{BlobStore, FsBlobStore};

/// Hands out the blob store with a given id.
#[async_trait]
pub trait BlobRouter: Send + Sync {
    async fn store(&self, id: StoreId) -> Result<Arc<dyn BlobStore>>;
}

/// Anything that can be used where a router is wanted: a single store (the platform store) or a router.
pub trait IntoRouter {
    fn into_router(self) -> Arc<dyn BlobRouter>;
}

impl IntoRouter for Arc<dyn BlobRouter> {
    fn into_router(self) -> Arc<dyn BlobRouter> {
        self
    }
}

impl IntoRouter for Arc<dyn BlobStore> {
    fn into_router(self) -> Arc<dyn BlobRouter> {
        Arc::new(SingleStore(self))
    }
}

impl IntoRouter for Arc<FsBlobStore> {
    fn into_router(self) -> Arc<dyn BlobRouter> {
        let store: Arc<dyn BlobStore> = self;
        Arc::new(SingleStore(store))
    }
}

/// One store serving as the platform store; asking for any other store is an error, never a silent fall back to the platform store.
pub struct SingleStore(pub Arc<dyn BlobStore>);

#[async_trait]
impl BlobRouter for SingleStore {
    async fn store(&self, id: StoreId) -> Result<Arc<dyn BlobStore>> {
        if id == PLATFORM_STORE {
            Ok(self.0.clone())
        } else {
            Err(Error::NotFound("blob store".into()))
        }
    }
}

/// Opens the store of a `blob_stores` row (never the platform row: that one is the configuration's).
#[async_trait]
pub trait StoreOpener: Send + Sync {
    async fn open(&self, row: &BlobStoreRow) -> Result<Arc<dyn BlobStore>>;
}

/// An opened store and when it was opened.
type OpenStore = (Arc<dyn BlobStore>, std::time::Instant);

/// How long an opened store is kept before its row (and, for a pool, its backends) is read again: a backend added, a state or a weight changed, keys replaced and a store that fell
/// out of its check all take effect within this time, without a restart.
pub const STORE_REFRESH: std::time::Duration = std::time::Duration::from_secs(60);

/// The stores of a deployment: the platform store and those of the table, opened lazily and kept for [`STORE_REFRESH`] (the platform store always). [`BlobStores::forget`] drops one at once.
pub struct BlobStores {
    platform: Arc<dyn BlobStore>,
    repo: Arc<dyn BlobStoreRepo>,
    opener: Arc<dyn StoreOpener>,
    open: RwLock<HashMap<StoreId, OpenStore>>,
    refresh: std::time::Duration,
}

impl BlobStores {
    pub fn new(platform: Arc<dyn BlobStore>, repo: Arc<dyn BlobStoreRepo>, opener: Arc<dyn StoreOpener>) -> BlobStores {
        BlobStores { platform, repo, opener, open: RwLock::new(HashMap::new()), refresh: STORE_REFRESH }
    }

    /// Keep opened stores for `refresh` instead of [`STORE_REFRESH`].
    pub fn with_refresh(mut self, refresh: std::time::Duration) -> BlobStores {
        self.refresh = refresh;
        self
    }

    pub fn forget(&self, id: StoreId) {
        self.open.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
    }

    fn fresh(&self, id: StoreId) -> Option<Arc<dyn BlobStore>> {
        let map = self.open.read().unwrap_or_else(|e| e.into_inner());
        map.get(&id).filter(|(_, at)| at.elapsed() < self.refresh).map(|(s, _)| s.clone())
    }
}

#[async_trait]
impl BlobRouter for BlobStores {
    async fn store(&self, id: StoreId) -> Result<Arc<dyn BlobStore>> {
        if id == PLATFORM_STORE {
            return Ok(self.platform.clone());
        }
        if let Some(s) = self.fresh(id) {
            return Ok(s);
        }
        let row = self.repo.get(id).await?.ok_or_else(|| Error::NotFound("blob store".into()))?;
        // A tenant's S3 store is used only after a check recorded `ok` (the settings were tried with the real keys, the bucket answered): until then, and after a check that failed,
        // mail for the domain is refused with a temporary failure (the sender retries) instead of being sent to a bucket nobody has seen work.
        if row.kind == "s3" && row.check_status != "ok" {
            return Err(Error::Busy("blob store not ready".into()));
        }
        let opened = self.opener.open(&row).await?;
        // Two tasks may have opened it at once: the first one in the map wins, the other is dropped.
        let mut map = self.open.write().unwrap_or_else(|e| e.into_inner());
        if let Some((s, at)) = map.get(&id) {
            if at.elapsed() < self.refresh {
                return Ok(s.clone());
            }
        }
        map.insert(id, (opened.clone(), std::time::Instant::now()));
        Ok(opened)
    }
}

/// Opens one backend of a pool (a bucket, or a directory in development).
#[async_trait]
pub trait BackendOpener: Send + Sync {
    async fn open(&self, row: &BackendRow) -> Result<Arc<dyn BlobStore>>;
}

/// Sends a backend row to the opener of its kind.
#[derive(Default)]
pub struct KindBackendOpener {
    fs: Option<Arc<dyn BackendOpener>>,
    s3: Option<Arc<dyn BackendOpener>>,
}

impl KindBackendOpener {
    pub fn new() -> KindBackendOpener {
        KindBackendOpener::default()
    }

    pub fn with_fs(mut self, opener: Arc<dyn BackendOpener>) -> KindBackendOpener {
        self.fs = Some(opener);
        self
    }

    pub fn with_s3(mut self, opener: Arc<dyn BackendOpener>) -> KindBackendOpener {
        self.s3 = Some(opener);
        self
    }
}

#[async_trait]
impl BackendOpener for KindBackendOpener {
    async fn open(&self, row: &BackendRow) -> Result<Arc<dyn BlobStore>> {
        let opener = match row.kind.as_str() {
            "fs" => self.fs.as_ref(),
            "s3" => self.s3.as_ref(),
            _ => None,
        };
        match opener {
            Some(o) => o.open(row).await,
            None => Err(Error::Invalid("this deployment cannot open that kind of backend".into())),
        }
    }
}

/// Opens a store of kind `pool`: its settings, its backends (each opened by the backend opener) and the breaker. A pool is used only when every backend that is not disabled passed a
/// check (a store whose backends nobody has seen work is refused with a temporary failure, as a single S3 store is).
pub struct PoolOpener {
    repo: Arc<dyn BlobPoolRepo>,
    backends: Arc<dyn BackendOpener>,
}

impl PoolOpener {
    pub fn new(repo: Arc<dyn BlobPoolRepo>, backends: Arc<dyn BackendOpener>) -> PoolOpener {
        PoolOpener { repo, backends }
    }
}

#[async_trait]
impl StoreOpener for PoolOpener {
    async fn open(&self, row: &BlobStoreRow) -> Result<Arc<dyn BlobStore>> {
        if row.kind != "pool" {
            return Err(Error::Invalid("not a blob pool".into()));
        }
        let policy = PoolPolicy::parse(&row.config)?;
        let rows: Vec<BackendRow> = self.repo.backends(row.id).await?.into_iter().filter(|b| b.state != BackendState::Disabled).collect();
        if rows.is_empty() {
            return Err(Error::Invalid("the pool has no backend".into()));
        }
        if rows.iter().any(|b| b.check_status != "ok") {
            return Err(Error::Busy("blob store not ready".into()));
        }
        let mut opened = Vec::with_capacity(rows.len());
        for b in rows {
            let store = self.backends.open(&b).await?;
            opened.push((b, store));
        }
        Ok(Arc::new(PoolStore::new(row.id, policy, self.repo.clone(), opened)?))
    }
}

/// Sends a row to the opener of its kind; a kind this deployment has no opener for is refused (the platform may not be able to reach S3, or may not allow directories).
#[derive(Default)]
pub struct KindOpener {
    fs: Option<Arc<dyn StoreOpener>>,
    s3: Option<Arc<dyn StoreOpener>>,
    pool: Option<Arc<dyn StoreOpener>>,
}

impl KindOpener {
    pub fn new() -> KindOpener {
        KindOpener::default()
    }

    pub fn with_fs(mut self, opener: Arc<dyn StoreOpener>) -> KindOpener {
        self.fs = Some(opener);
        self
    }

    pub fn with_s3(mut self, opener: Arc<dyn StoreOpener>) -> KindOpener {
        self.s3 = Some(opener);
        self
    }

    pub fn with_pool(mut self, opener: Arc<dyn StoreOpener>) -> KindOpener {
        self.pool = Some(opener);
        self
    }
}

#[async_trait]
impl StoreOpener for KindOpener {
    async fn open(&self, row: &BlobStoreRow) -> Result<Arc<dyn BlobStore>> {
        let opener = match row.kind.as_str() {
            "fs" => self.fs.as_ref(),
            "s3" => self.s3.as_ref(),
            "pool" => self.pool.as_ref(),
            _ => None,
        };
        match opener {
            Some(o) => o.open(row).await,
            None => Err(Error::Invalid("this deployment cannot open that kind of blob store".into())),
        }
    }
}

/// Stores of kind `fs`: a directory of its own under a base directory the operator names (`{"dir": "<name>"}`, one plain path component, created with the usual
/// protections). Kind `s3` is opened by the S3 opener (a later step); this one refuses it.
pub struct FsStoreOpener {
    base: PathBuf,
}

impl FsStoreOpener {
    pub fn new(base: impl Into<PathBuf>) -> FsStoreOpener {
        FsStoreOpener { base: base.into() }
    }
}

/// `{"dir": "name"}` with a name of 1 to 64 letters, digits, `-` and `_`.
fn fs_dir_name(config: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(config).map_err(|_| Error::Invalid("the blob store settings are not JSON".into()))?;
    let name = v.get("dir").and_then(serde_json::Value::as_str).unwrap_or_default();
    if (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        Ok(name.to_owned())
    } else {
        Err(Error::Invalid("a filesystem blob store needs a \"dir\" of 1 to 64 letters, digits, - and _".into()))
    }
}

#[async_trait]
impl StoreOpener for FsStoreOpener {
    async fn open(&self, row: &BlobStoreRow) -> Result<Arc<dyn BlobStore>> {
        if row.kind != "fs" {
            return Err(Error::Invalid("this deployment opens only filesystem blob stores".into()));
        }
        self.open_dir(&row.config).await
    }
}

#[async_trait]
impl BackendOpener for FsStoreOpener {
    async fn open(&self, row: &BackendRow) -> Result<Arc<dyn BlobStore>> {
        if row.kind != "fs" {
            return Err(Error::Invalid("this opener opens only filesystem backends".into()));
        }
        self.open_dir(&row.config).await
    }
}

impl FsStoreOpener {
    async fn open_dir(&self, config: &str) -> Result<Arc<dyn BlobStore>> {
        let dir = self.base.join(fs_dir_name(config)?);
        // The base directory is made private if it does not exist yet; each store's own directory is initialised (mode 0700, marker) by `FsBlobStore::init`.
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.base).map_err(|e| Error::storage_with("cannot create the blob stores directory", e))?;
        }
        FsBlobStore::init(&dir).await?;
        Ok(Arc::new(FsBlobStore::open(&dir).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filesystem_store_directory_is_one_plain_name() {
        assert_eq!(fs_dir_name(r#"{"dir":"tenant-1_a"}"#).unwrap(), "tenant-1_a");
        for bad in [r#"{}"#, r#"{"dir":""}"#, r#"{"dir":"../x"}"#, r#"{"dir":"a/b"}"#, r#"{"dir":"."}"#, r#"{"dir":".."}"#, r#"{"dir":"a b"}"#, "not json"] {
            assert!(fs_dir_name(bad).is_err(), "{bad}");
        }
        assert!(fs_dir_name(&format!(r#"{{"dir":"{}"}}"#, "a".repeat(65))).is_err());
    }
}
