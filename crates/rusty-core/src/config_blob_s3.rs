//! `[blob_s3]`: the platform blob store in an S3 bucket instead of the `blob_dir` directory (docs/blob-stores.md).
//!
//! The operator owns this section, so unlike a tenant's store the endpoint may be `http://` and may name a private address (a MinIO or RustFS on the same network). The keys are
//! not in the configuration: they are read, through the secret source, from the two files named here (mode 0600, one line each). The directory `blob_dir` stays in the
//! configuration (it is where the other stores of the deployment live) but holds no message body while `[blob_s3]` is set.
//!
//! This module checks the shape only; the names (region, bucket, prefix) are checked by the S3 client when the store is opened at start-up, so a wrong one stops the start.

use std::path::PathBuf;

use serde::Deserialize;

use crate::{Error, Result};

/// Requests in flight at once to the bucket, unless `max_concurrent` says otherwise.
pub const DEFAULT_BLOB_S3_CONCURRENT: usize = 64;

/// `[blob_s3]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobS3Config {
    /// `https://host[:port]` or `http://host[:port]`, nothing after the port.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Every object is named under this prefix (none: the bucket root).
    #[serde(default)]
    pub prefix: String,
    /// `https://host/bucket/key` (true) or `https://bucket.host/key` (false, the default).
    #[serde(default)]
    pub path_style: bool,
    /// Files holding the access key and the secret key.
    pub access_key_file: PathBuf,
    pub secret_key_file: PathBuf,
    /// Requests in flight at once (1 to 1024).
    #[serde(default = "default_concurrent")]
    pub max_concurrent: usize,
    /// Seconds a request may take, queueing for its turn included.
    #[serde(default = "default_deadline")]
    pub request_timeout_secs: u64,
}

fn default_concurrent() -> usize {
    DEFAULT_BLOB_S3_CONCURRENT
}

fn default_deadline() -> u64 {
    60
}

impl BlobS3Config {
    pub(crate) fn validate(&self) -> Result<()> {
        let rest = self.endpoint.strip_prefix("https://").or_else(|| self.endpoint.strip_prefix("http://"));
        let ok = rest.is_some_and(|r| {
            let r = r.strip_suffix('/').unwrap_or(r);
            !r.is_empty() && !r.contains(['/', '?', '#', '@', ' ', '\\']) && !r.chars().any(char::is_control)
        });
        if !ok {
            return Err(Error::Invalid("blob_s3.endpoint must be http(s)://host[:port], nothing more".into()));
        }
        if !self.access_key_file.is_absolute() || !self.secret_key_file.is_absolute() || self.access_key_file == self.secret_key_file {
            return Err(Error::Invalid("blob_s3 key files must be two distinct absolute paths".into()));
        }
        if !(1..=1024).contains(&self.max_concurrent) {
            return Err(Error::Invalid("blob_s3.max_concurrent must be 1 to 1024".into()));
        }
        if !(1..=3600).contains(&self.request_timeout_secs) {
            return Err(Error::Invalid("blob_s3.request_timeout_secs must be 1 to 3600".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> BlobS3Config {
        BlobS3Config {
            endpoint: "http://10.0.0.5:9000".into(),
            region: "us-east-1".into(),
            bucket: "rusty-mail".into(),
            prefix: String::new(),
            path_style: true,
            access_key_file: "/run/secrets/s3-access".into(),
            secret_key_file: "/run/secrets/s3-secret".into(),
            max_concurrent: 64,
            request_timeout_secs: 60,
        }
    }

    #[test]
    fn an_operators_endpoint_may_be_plain_http_and_private_but_not_more_than_host_and_port() {
        assert!(base().validate().is_ok());
        for bad in ["ftp://h", "h", "https://", "http://h/x", "https://u@h", "https://h?x=1", "https://h h", ""] {
            assert!(BlobS3Config { endpoint: bad.into(), ..base() }.validate().is_err(), "{bad}");
        }
        assert!(BlobS3Config { endpoint: "https://s3.example.com/".into(), ..base() }.validate().is_ok());
    }

    #[test]
    fn the_key_files_limits_and_timeout_are_checked() {
        assert!(BlobS3Config { access_key_file: "relative".into(), ..base() }.validate().is_err());
        assert!(BlobS3Config { secret_key_file: base().access_key_file, ..base() }.validate().is_err());
        assert!(BlobS3Config { max_concurrent: 0, ..base() }.validate().is_err());
        assert!(BlobS3Config { max_concurrent: 1025, ..base() }.validate().is_err());
        assert!(BlobS3Config { request_timeout_secs: 0, ..base() }.validate().is_err());
    }
}
