//! The platform blob store in a bucket (`[blob_s3]`, docs/blob-stores.md): built from the operator's configuration, keys read through the secret source.
//!
//! Unlike a tenant's store the operator chose this endpoint, so the connection goes straight to it (`DirectConnector`): a private address and plain `http://` are allowed. The names
//! are still checked by the client, and the keys are never logged or kept in the configuration.

use std::sync::Arc;
use std::time::Duration;

use rusty_core::config::BlobS3Config;
use rusty_core::{Error, Result};
use rusty_svc::SecretSource;

use crate::client::{tls_config, Credentials, S3Client, S3Config};
use crate::http::DirectConnector;
use crate::store::S3BlobStore;

fn key_text(secrets: &dyn SecretSource, path: &std::path::Path, what: &str) -> Result<String> {
    let bytes = secrets.read(path)?;
    let text = String::from_utf8(bytes).map_err(|_| Error::Invalid(format!("the S3 {what} file is not text")))?;
    // One line: a trailing newline is the file's, not the key's.
    Ok(text.trim_end_matches(['\n', '\r']).to_owned())
}

/// The client of the platform bucket (also what a check of it uses).
pub fn platform_client(cfg: &BlobS3Config, secrets: &dyn SecretSource) -> Result<S3Client> {
    let creds = Credentials { access_key: key_text(secrets, &cfg.access_key_file, "access key")?, secret_key: key_text(secrets, &cfg.secret_key_file, "secret key")? };
    let s3 = S3Config {
        endpoint: cfg.endpoint.trim_end_matches('/').to_owned(),
        region: cfg.region.clone(),
        bucket: cfg.bucket.clone(),
        prefix: cfg.prefix.clone(),
        path_style: cfg.path_style,
        deadline: Duration::from_secs(cfg.request_timeout_secs),
    };
    let tls = if cfg.endpoint.starts_with("https://") { Some(tls_config(None)?) } else { None };
    Ok(S3Client::new(s3, creds, Arc::new(DirectConnector { timeout: Duration::from_secs(5) }), tls)?.with_max_concurrent(cfg.max_concurrent))
}

/// The platform store over the configured bucket.
pub fn platform_store(cfg: &BlobS3Config, secrets: &dyn SecretSource) -> Result<S3BlobStore> {
    Ok(S3BlobStore::new(platform_client(cfg, secrets)?))
}
