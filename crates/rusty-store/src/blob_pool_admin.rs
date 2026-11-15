//! `BlobPoolAdmin`: what an administrator may do with a domain's pool of backends (docs/design-s3-per-tenant.md, section 12): make the domain's store a pool with a policy, add S3
//! backends (settings checked by `rusty_core::storage_policy`, credentials sealed through the sealed facades), change their state and weight, remove one that holds nothing, record the
//! result of a check. The table's constraints are the second line.

use std::sync::Arc;

use rusty_core::storage_policy::TenantS3;
use rusty_core::{DomainId, Error, Result};

use crate::blob_store_admin::CHECK_STATUSES;
use crate::pool::PoolPolicy;
use crate::repo::{BackendId, BackendRow, BackendState, BlobPoolRepo, BlobStoreRepo, BlobStoreRow, StoreId};

#[derive(Clone)]
pub struct BlobPoolAdmin {
    stores: Arc<dyn BlobStoreRepo>,
    pool: Arc<dyn BlobPoolRepo>,
}

fn s3_json(s3: &TenantS3) -> String {
    serde_json::json!({ "endpoint": s3.endpoint, "region": s3.region, "bucket": s3.bucket, "prefix": s3.prefix, "path_style": s3.path_style }).to_string()
}

fn weight_ok(weight: u32) -> Result<u32> {
    if (1..=1000).contains(&weight) {
        Ok(weight)
    } else {
        Err(Error::Invalid("a backend's weight is 1 to 1000".into()))
    }
}

impl BlobPoolAdmin {
    pub fn with_repos(stores: Arc<dyn BlobStoreRepo>, pool: Arc<dyn BlobPoolRepo>) -> BlobPoolAdmin {
        BlobPoolAdmin { stores, pool }
    }

    /// Make `domain`'s store a pool with this policy, or change the policy of the pool it has (while no body is stored: the settings of a store with bodies do not change). Returns the
    /// row. `Conflict` for a domain that has a store of another kind or holds messages on the platform store.
    pub async fn set_policy(&self, domain: DomainId, policy: PoolPolicy) -> Result<BlobStoreRow> {
        let config = policy.to_json();
        match self.stores.for_domain(domain).await? {
            Some(row) if row.kind != "pool" => Err(Error::Conflict("the domain's blob store is not a pool".into())),
            Some(row) => {
                let same = serde_json::from_str::<serde_json::Value>(&row.config).ok() == serde_json::from_str::<serde_json::Value>(&config).ok();
                if !same {
                    self.stores.update_config(row.id, &config).await?;
                }
                self.stores.get(row.id).await?.ok_or_else(|| Error::NotFound("blob store".into()))
            }
            None => {
                let id = self.stores.create_for_domain(domain, "pool", &config).await?;
                self.stores.get(id).await?.ok_or_else(|| Error::NotFound("blob store".into()))
            }
        }
    }

    /// The pool of a domain, if its store is one.
    pub async fn pool_of(&self, domain: DomainId) -> Result<Option<BlobStoreRow>> {
        Ok(self.stores.for_domain(domain).await?.filter(|r| r.kind == "pool"))
    }

    pub async fn backends(&self, store: StoreId) -> Result<Vec<BackendRow>> {
        self.pool.backends(store).await
    }

    pub async fn backend(&self, id: BackendId) -> Result<Option<BackendRow>> {
        self.pool.backend(id).await
    }

    /// Add an S3 backend named `name` to the pool `store`. The policy has to stay possible (`copies` cannot exceed the number of backends that are not disabled once the pool is used;
    /// that is checked when the pool is opened, a pool being built up has fewer than `copies` for a while). Returns the new backend, `unchecked`.
    pub async fn add_s3(&self, store: StoreId, name: &str, s3: &TenantS3, weight: u32) -> Result<BackendRow> {
        let id = self.pool.add_backend(store, name, "s3", &s3_json(s3), weight_ok(weight)?).await?;
        // A pool with a backend nobody has checked is not ready.
        self.stores.record_check(store, "unchecked", None).await?;
        self.pool.backend(id).await?.ok_or_else(|| Error::NotFound("blob backend".into()))
    }

    /// New settings for a backend that holds no object, or the same settings again (the keys are being replaced): either way the backend has to pass a check again.
    pub async fn set_s3(&self, id: BackendId, s3: &TenantS3) -> Result<BackendRow> {
        let row = self.pool.backend(id).await?.ok_or_else(|| Error::NotFound("blob backend".into()))?;
        if row.kind != "s3" {
            return Err(Error::Conflict("the backend is not an S3 backend".into()));
        }
        let config = s3_json(s3);
        let same = serde_json::from_str::<serde_json::Value>(&row.config).ok() == serde_json::from_str::<serde_json::Value>(&config).ok();
        if same {
            self.pool.record_backend_check(id, "unchecked", None).await?;
        } else {
            self.pool.update_backend_config(id, &config).await?;
        }
        self.stores.record_check(row.store, "unchecked", None).await?;
        self.pool.backend(id).await?.ok_or_else(|| Error::NotFound("blob backend".into()))
    }

    pub async fn set_state(&self, id: BackendId, state: BackendState) -> Result<()> {
        self.pool.set_backend_state(id, state).await
    }

    pub async fn set_weight(&self, id: BackendId, weight: u32) -> Result<()> {
        self.pool.set_backend_weight(id, weight_ok(weight)?).await
    }

    /// Remove a backend that holds no object (`Conflict` otherwise: drain it first).
    pub async fn remove(&self, id: BackendId) -> Result<()> {
        self.pool.remove_backend(id).await
    }

    /// How many objects a backend holds.
    pub async fn count_held(&self, id: BackendId) -> Result<i64> {
        self.pool.count_held(id).await
    }

    /// Record the result of a check of a backend: one of the fixed categories.
    pub async fn record_check(&self, id: BackendId, status: &str) -> Result<()> {
        if !CHECK_STATUSES.contains(&status) {
            return Err(Error::Invalid("unknown check status".into()));
        }
        self.pool.record_backend_check(id, status, None).await
    }

    /// Record the result of the check of the whole pool (every backend that is not disabled): `ok` only when each one is, else the category of the first that is not.
    pub async fn record_pool_check(&self, store: StoreId) -> Result<()> {
        let backends = self.pool.backends(store).await?;
        let used: Vec<&BackendRow> = backends.iter().filter(|b| b.state != BackendState::Disabled).collect();
        let status = if used.is_empty() { "config" } else { used.iter().find(|b| b.check_status != "ok").map_or("ok", |b| b.check_status.as_str()) };
        self.stores.record_check(store, status, None).await
    }
}
