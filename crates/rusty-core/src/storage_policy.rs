//! The rules for a blob store that a **tenant** chooses (docs/design-s3-per-tenant.md, section 6). A tenant decides where the services make requests, so the endpoint is a
//! server-side-request-forgery surface: these pure functions say what is acceptable and do no I/O. The caller checks the **resolved addresses** with
//! [`crate::relay_policy::is_public_address`] at every connection, not only when the store is entered.
//!
//! Refusal texts are fixed and never echo the input.

use crate::relay_policy::normalize_host;
use crate::{Error, Result};

/// `sealed_secrets.owner_kind` of a blob store's credentials (owner id: the store's id).
pub const STORE_SECRET_OWNER: &str = "blobstore";
/// `sealed_secrets.purpose` of the access key and of the secret key of an S3 store.
pub const ACCESS_KEY_PURPOSE: &str = "s3-access-key";
pub const SECRET_KEY_PURPOSE: &str = "s3-secret-key";

/// The job kind of a store check (queued by the admin web or `rustyctl`, run by the service that holds the unsealing key). The payload is `{"store": <store id>}`.
pub const STORAGE_CHECK_JOB_KIND: &str = "storage.check";

/// The ports a tenant may name for an S3 endpoint: HTTPS, its usual alternative, and the ports the common self-hosted S3 servers use.
pub const TENANT_S3_PORTS: [u16; 4] = [443, 8443, 9000, 9443];

/// A tenant's S3 store as stated, after the checks. Credentials are not part of it (they are sealed separately).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantS3 {
    /// `https://host` or `https://host:port`, canonical (lower case, no trailing slash).
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Key prefix without leading or trailing `/`; may be empty.
    pub prefix: String,
    /// `endpoint/bucket/key` (true) or `bucket.endpoint/key` (false).
    pub path_style: bool,
}

fn invalid(why: &str) -> Error {
    Error::Invalid(why.into())
}

/// Check what a tenant entered. The endpoint must be `https://` with a public DNS name (or public IP literal), no credentials, path, query or fragment, and a port from
/// [`TENANT_S3_PORTS`]; the region, bucket and prefix are S3 names that cannot smuggle a path or another host.
pub fn validate_tenant_s3(endpoint: &str, region: &str, bucket: &str, prefix: &str, path_style: bool) -> Result<TenantS3> {
    let rest = endpoint.strip_prefix("https://").ok_or_else(|| invalid("the S3 endpoint must start with https://"))?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() || rest.contains(['/', '?', '#', '@', ' ', '\\']) || rest.chars().any(char::is_control) {
        return Err(invalid("the S3 endpoint must be https://host or https://host:port, nothing more"));
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h, p.parse::<u16>().map_err(|_| invalid("the S3 endpoint port is not a number"))?),
        Some(_) => return Err(invalid("an IPv6 literal is not accepted as an S3 endpoint")),
        None => (rest, 443),
    };
    if !TENANT_S3_PORTS.contains(&port) {
        return Err(invalid("the S3 endpoint port must be 443, 8443, 9000 or 9443"));
    }
    let host = normalize_host(host)?;
    let endpoint = if port == 443 { format!("https://{host}") } else { format!("https://{host}:{port}") };

    if !(1..=32).contains(&region.len()) || !region.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err(invalid("the S3 region must be 1 to 32 lower-case letters, digits and -"));
    }
    let b = bucket.as_bytes();
    let bucket_ok = (3..=63).contains(&b.len())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'.')
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && !bucket.contains("..")
        && bucket.parse::<std::net::Ipv4Addr>().is_err();
    if !bucket_ok {
        return Err(invalid("the S3 bucket name is not a valid bucket name"));
    }
    let prefix_ok = prefix.len() <= 256
        && prefix.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'/'))
        && !prefix.starts_with('/')
        && !prefix.ends_with('/')
        && !prefix.split('/').any(|s| s == ".." || s == "." || (s.is_empty() && !prefix.is_empty()));
    if !prefix_ok {
        return Err(invalid("the S3 key prefix must be letters, digits, - _ . and single slashes, without . or .. parts"));
    }
    Ok(TenantS3 { endpoint, region: region.to_owned(), bucket: bucket.to_owned(), prefix: prefix.to_owned(), path_style })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(e: &str) -> TenantS3 {
        validate_tenant_s3(e, "us-east-1", "tenant-mail", "mail/a", true).unwrap()
    }

    #[test]
    fn a_public_https_endpoint_is_accepted_and_made_canonical() {
        assert_eq!(ok("https://S3.Example-Cloud.com").endpoint, "https://s3.example-cloud.com");
        assert_eq!(ok("https://s3.example-cloud.com/").endpoint, "https://s3.example-cloud.com");
        assert_eq!(ok("https://s3.example-cloud.com:9000").endpoint, "https://s3.example-cloud.com:9000");
        assert_eq!(ok("https://s3.example-cloud.com:443").endpoint, "https://s3.example-cloud.com");
        assert_eq!(ok("https://8.8.8.8").endpoint, "https://8.8.8.8", "a public address literal");
    }

    #[test]
    fn everything_that_points_inside_or_smuggles_something_is_refused() {
        let bad = |e: &str| validate_tenant_s3(e, "us-east-1", "tenant-mail", "", true).is_err();
        for e in [
            "http://s3.example-cloud.com",
            "s3.example-cloud.com",
            "ftp://s3.example-cloud.com",
            "https://",
            "https://s3.example-cloud.com/bucket",
            "https://user:pw@s3.example-cloud.com",
            "https://s3.example-cloud.com?x=1",
            "https://s3.example-cloud.com#f",
            "https://s3.example-cloud.com:22",
            "https://s3.example-cloud.com:8080",
            "https://s3.example-cloud.com:99999",
            "https://s3.example-cloud.com:abc",
            "https://127.0.0.1",
            "https://10.0.0.5:9000",
            "https://169.254.169.254",
            "https://[::1]:443",
            "https://localhost",
            "https://minio.internal",
            "https://s3",
            "https://0x7f.1",
            "https://s3.example-cloud.com\r\nHost: evil",
            "https://s3.example-cloud.com\\@evil",
        ] {
            assert!(bad(e), "{e:?} must be refused");
        }
    }

    #[test]
    fn names_of_a_region_a_bucket_and_a_prefix_cannot_smuggle_anything() {
        let t = |region: &str, bucket: &str, prefix: &str| validate_tenant_s3("https://s3.example-cloud.com", region, bucket, prefix, false);
        assert!(t("eu-west-3", "my.bucket-1", "").is_ok());
        assert!(t("eu-west-3", "my-bucket", "a/b_c.d-e").is_ok());
        for region in ["", "US-East", "us east", "a".repeat(33).as_str(), "us/east"] {
            assert!(t(region, "my-bucket", "").is_err(), "{region:?}");
        }
        for bucket in ["ab", "UPPER", "-abc", "abc-", "a..b", "192.168.0.1", "my_bucket", "a/b", &"a".repeat(64)] {
            assert!(t("r", bucket, "").is_err(), "{bucket:?}");
        }
        for prefix in ["/x", "x/", "a//b", "../x", "a/../b", ".", "a b", "a?b", "a\nb"] {
            assert!(t("r", "my-bucket", prefix).is_err(), "{prefix:?}");
        }
    }
}
