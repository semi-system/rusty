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

use crate::repo::{BlobStoreRepo, BlobStoreRow, StoreId, PLATFORM_STORE};
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

/// The stores of a deployment: the platform store and those of the table, opened lazily and kept. A store row is not changed once bodies were written to it
/// (version 1), so a kept store never goes stale; [`BlobStores::forget`] drops one (after a settings change in a later version).
pub struct BlobStores {
    platform: Arc<dyn BlobStore>,
    repo: Arc<dyn BlobStoreRepo>,
    opener: Arc<dyn StoreOpener>,
    open: RwLock<HashMap<StoreId, Arc<dyn BlobStore>>>,
}

impl BlobStores {
    pub fn new(platform: Arc<dyn BlobStore>, repo: Arc<dyn BlobStoreRepo>, opener: Arc<dyn StoreOpener>) -> BlobStores {
        BlobStores { platform, repo, opener, open: RwLock::new(HashMap::new()) }
    }

    pub fn forget(&self, id: StoreId) {
        self.open.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
    }
}

#[async_trait]
impl BlobRouter for BlobStores {
    async fn store(&self, id: StoreId) -> Result<Arc<dyn BlobStore>> {
        if id == PLATFORM_STORE {
            return Ok(self.platform.clone());
        }
        if let Some(s) = self.open.read().unwrap_or_else(|e| e.into_inner()).get(&id) {
            return Ok(s.clone());
        }
        let row = self.repo.get(id).await?.ok_or_else(|| Error::NotFound("blob store".into()))?;
        let opened = self.opener.open(&row).await?;
        // Two tasks may have opened it at once: the first one in the map wins, the other is dropped.
        let mut map = self.open.write().unwrap_or_else(|e| e.into_inner());
        Ok(map.entry(id).or_insert(opened).clone())
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
        let dir = self.base.join(fs_dir_name(&row.config)?);
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
