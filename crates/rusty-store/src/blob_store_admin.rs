//! `BlobStoreAdmin`: what an administrator may do with the blob store of a domain (docs/design-s3-per-tenant.md, section 7): give it an S3 store of its own, change the settings while
//! the store is still empty, record the result of a check. The checks of what may be stored belong here (`rusty_core::storage_policy`); the table's constraints are the second line.
//! The credentials are sealed secrets written through the sealed facades, never here.

use std::sync::Arc;

use rusty_core::storage_policy::TenantS3;
use rusty_core::{DomainId, Error, Result};

use crate::repo::{BlobStoreRepo, BlobStoreRow, StoreId};

/// The categories a check may record besides `ok` (the names of `rusty_s3::S3Fail`, plus the one of a store that cannot be opened at all).
pub const CHECK_STATUSES: [&str; 15] = [
    "unchecked", "ok", "config", "resolve", "connect", "tls", "timeout", "auth", "denied", "no-bucket", "not-found", "throttled", "server", "protocol", "too-large",
];

#[derive(Clone)]
pub struct BlobStoreAdmin {
    repo: Arc<dyn BlobStoreRepo>,
}

fn config_json(s3: &TenantS3) -> String {
    serde_json::json!({ "endpoint": s3.endpoint, "region": s3.region, "bucket": s3.bucket, "prefix": s3.prefix, "path_style": s3.path_style }).to_string()
}

impl BlobStoreAdmin {
    pub fn with_repo(repo: Arc<dyn BlobStoreRepo>) -> BlobStoreAdmin {
        BlobStoreAdmin { repo }
    }

    /// The store the domain owns, if any (a domain without one uses the platform store).
    pub async fn for_domain(&self, domain: DomainId) -> Result<Option<BlobStoreRow>> {
        self.repo.for_domain(domain).await
    }

    pub async fn get(&self, id: StoreId) -> Result<Option<BlobStoreRow>> {
        self.repo.get(id).await
    }

    /// Give `domain` an S3 store with these settings, or change the settings of the one it has while it holds no body. The same settings again change nothing (that is how credentials
    /// are rotated). Returns the row and whether it was made now. `Conflict` for a domain that already holds messages on the platform store, for a store of another kind and for a
    /// store that already holds bodies.
    pub async fn set_s3(&self, domain: DomainId, s3: &TenantS3) -> Result<(BlobStoreRow, bool)> {
        let config = config_json(s3);
        match self.repo.for_domain(domain).await? {
            Some(row) if row.kind != "s3" => Err(Error::Conflict("the domain's blob store is not an S3 store".into())),
            Some(row) => {
                let same = serde_json::from_str::<serde_json::Value>(&row.config).ok() == serde_json::from_str::<serde_json::Value>(&config).ok();
                if !same {
                    self.repo.update_config(row.id, &config).await?;
                } else {
                    // The same settings: the keys are being replaced. The store has to pass a check again before it is used (after the next start of the services).
                    self.repo.record_check(row.id, "unchecked", None).await?;
                }
                let fresh = self.repo.get(row.id).await?.ok_or_else(|| Error::NotFound("blob store".into()))?;
                Ok((fresh, false))
            }
            None => {
                let id = self.repo.create_for_domain(domain, "s3", &config).await?;
                let row = self.repo.get(id).await?.ok_or_else(|| Error::NotFound("blob store".into()))?;
                Ok((row, true))
            }
        }
    }

    /// Record the result of a check: one of [`CHECK_STATUSES`].
    pub async fn record_check(&self, id: StoreId, status: &str) -> Result<()> {
        if !CHECK_STATUSES.contains(&status) {
            return Err(Error::Invalid("unknown check status".into()));
        }
        self.repo.record_check(id, status, None).await
    }

    #[doc(hidden)]
    pub async fn record_check_at(&self, id: StoreId, status: &str, now: i64) -> Result<()> {
        if !CHECK_STATUSES.contains(&status) {
            return Err(Error::Invalid("unknown check status".into()));
        }
        self.repo.record_check(id, status, Some(now)).await
    }
}
