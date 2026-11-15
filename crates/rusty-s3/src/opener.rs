//! `S3StoreOpener`: opens the store of a `blob_stores` row of kind `s3`. The non-secret settings come from the row, the access key and the secret key from sealed secrets (owner kind
//! `blobstore`, the store's id, purposes [`ACCESS_KEY_PURPOSE`] and [`SECRET_KEY_PURPOSE`]) opened with this service's unsealing key; the endpoint is checked again against the tenant
//! rules (a row written by any other path is not trusted) and every connection goes through the address policy of [`PolicyConnector`].
//!
//! A store without credentials, or whose credentials cannot be opened, is an error and never a connection without them.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::storage_policy::validate_tenant_s3;
use rusty_core::{Error, Result};
use rusty_store::repo::{BlobStoreRow, StoreId};
use rusty_store::{BlobStore, SealedReader, StoreOpener};
use serde::Deserialize;

use crate::client::{Credentials, S3Client, S3Config};
use crate::http::Connector;
use crate::policy::PolicyConnector;
use crate::store::S3BlobStore;

pub use rusty_core::storage_policy::{ACCESS_KEY_PURPOSE, SECRET_KEY_PURPOSE, STORE_SECRET_OWNER as OWNER_KIND};

/// The settings of a store as stored in `blob_stores.config`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredConfig {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub path_style: bool,
}

pub struct S3StoreOpener {
    reader: SealedReader,
    unsealer: Arc<rusty_seal::Unsealer>,
    tls: Arc<rustls::ClientConfig>,
    connector: Arc<dyn Connector>,
    deadline: Duration,
}

impl S3StoreOpener {
    /// The opener for tenant stores: addresses are checked at every connection, TLS is verified against `tls` (the web roots).
    pub fn new(reader: SealedReader, unsealer: Arc<rusty_seal::Unsealer>, tls: Arc<rustls::ClientConfig>) -> S3StoreOpener {
        S3StoreOpener { reader, unsealer, tls, connector: Arc::new(PolicyConnector { timeout: Duration::from_secs(5) }), deadline: Duration::from_secs(60) }
    }

    async fn secret(&self, store: StoreId, purpose: &str) -> Result<String> {
        let row = self.reader.get(OWNER_KIND, store.0, purpose).await?.ok_or_else(|| Error::Invalid("the blob store has no credentials".into()))?;
        let context = rusty_seal::Context::new(OWNER_KIND, store.0, purpose)?;
        let plain = self.unsealer.unseal(&context, &row.sealed)?;
        String::from_utf8(plain.to_vec()).map_err(|_| Error::Invalid("the blob store credentials are not text".into()))
    }

    /// The client of a store row, without the `BlobStore` wrapper (the check uses it).
    pub async fn client(&self, row: &BlobStoreRow) -> Result<S3Client> {
        if row.kind != "s3" {
            return Err(Error::Invalid("not an S3 blob store".into()));
        }
        let stored: StoredConfig = serde_json::from_str(&row.config).map_err(|_| Error::Invalid("the blob store settings are not valid".into()))?;
        let t = validate_tenant_s3(&stored.endpoint, &stored.region, &stored.bucket, &stored.prefix, stored.path_style)?;
        let creds = Credentials { access_key: self.secret(row.id, ACCESS_KEY_PURPOSE).await?, secret_key: self.secret(row.id, SECRET_KEY_PURPOSE).await? };
        let cfg = S3Config { endpoint: t.endpoint, region: t.region, bucket: t.bucket, prefix: t.prefix, path_style: t.path_style, deadline: self.deadline };
        S3Client::new(cfg, creds, self.connector.clone(), Some(self.tls.clone()))
    }
}

#[async_trait]
impl StoreOpener for S3StoreOpener {
    async fn open(&self, row: &BlobStoreRow) -> Result<Arc<dyn BlobStore>> {
        Ok(Arc::new(S3BlobStore::new(self.client(row).await?)))
    }
}
