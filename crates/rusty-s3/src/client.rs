//! The S3 calls this system needs, over one bucket: put, get (whole or a range), head, delete and list. Every call is signed (Signature V4, the payload hash included), has a
//! deadline and a size limit, and fails with one of the fixed categories of [`S3Fail`].

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use rusty_core::{Error, Result};

use crate::error::S3Fail;
use crate::http::{Connector, Endpoint, Http, Reply};
use crate::sigv4::{authorization, sha256_hex, uri_encode, Request};
use crate::time::{amz_dates, parse_http_date, parse_iso8601};
use crate::xml;

/// The most objects a listing page holds (S3's own maximum).
const PAGE: usize = 1000;
/// The biggest reply a call that is not a get accepts (a listing page, an error document).
const MAX_SMALL_REPLY: usize = 4 * 1024 * 1024;

/// An access key pair. `Debug` never shows the secret.
#[derive(Clone)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials").field("access_key", &self.access_key).field("secret_key", &"<redacted>").finish()
    }
}

/// Where and how to talk to one bucket.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// `https://host[:port]` (`http://` only for a store the operator runs on a private network).
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Key prefix, without leading or trailing `/`; may be empty.
    pub prefix: String,
    /// `endpoint/bucket/key` (true) or `bucket.endpoint/key` (false).
    pub path_style: bool,
    /// The whole of one request.
    pub deadline: Duration,
}

impl S3Config {
    /// The checks that do not need the network: a name for each part that cannot smuggle a path, a header or another host.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::Invalid(format!("invalid S3 {what}")));
        if Endpoint::parse(&self.endpoint).is_none() {
            return bad("endpoint");
        }
        if !(1..=32).contains(&self.region.len()) || !self.region.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return bad("region");
        }
        let b = self.bucket.as_bytes();
        let bucket_ok = (3..=63).contains(&b.len())
            && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'.')
            && b[0].is_ascii_alphanumeric()
            && b[b.len() - 1].is_ascii_alphanumeric()
            && !self.bucket.contains("..");
        if !bucket_ok {
            return bad("bucket name");
        }
        let prefix_ok = self.prefix.len() <= 256
            && self.prefix.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'/'))
            && !self.prefix.starts_with('/')
            && !self.prefix.ends_with('/')
            && !self.prefix.split('/').any(|s| s == ".." || s == "." || (s.is_empty() && !self.prefix.is_empty()));
        if !prefix_ok {
            return bad("key prefix");
        }
        Ok(())
    }
}

/// What a HEAD tells about an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectMeta {
    pub size: u64,
    pub modified_unix: i64,
}

/// One object of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub key: String,
    pub size: u64,
    pub modified_unix: i64,
}

/// One page of a listing; `next` is the token of the following page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListPage {
    pub entries: Vec<ListEntry>,
    pub next: Option<String>,
}

pub struct S3Client {
    http: Http,
    cfg: S3Config,
    creds: Credentials,
    /// Seconds the server's clock is ahead of ours, from the `Date` header of the last reply: signatures use the server's time (a skew over a few minutes is refused), and GC
    /// compares the store's clock with the database clock.
    offset: AtomicI64,
    /// At most this many requests are in flight at once (a store of a pool has its own client, so each backend has its own cap): a burst of deliveries waits here instead of opening
    /// hundreds of connections to the bucket. A request that waits longer than the deadline fails as a timeout.
    permits: Arc<tokio::sync::Semaphore>,
}

/// Requests in flight at once per client, unless [`S3Client::with_max_concurrent`] says otherwise.
pub const DEFAULT_MAX_CONCURRENT: usize = 64;

impl fmt::Debug for S3Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Client").field("endpoint", &self.cfg.endpoint).field("bucket", &self.cfg.bucket).finish_non_exhaustive()
    }
}

impl S3Client {
    pub fn new(cfg: S3Config, creds: Credentials, connector: Arc<dyn Connector>, tls: Option<Arc<rustls::ClientConfig>>) -> Result<S3Client> {
        cfg.validate()?;
        if creds.access_key.is_empty() || creds.secret_key.is_empty() || creds.access_key.len() > 128 || creds.secret_key.len() > 256 || creds.access_key.contains(char::is_control) || creds.secret_key.contains(char::is_control) {
            return Err(Error::Invalid("invalid S3 credentials".into()));
        }
        let endpoint = Endpoint::parse(&cfg.endpoint).ok_or_else(|| Error::Invalid("invalid S3 endpoint".into()))?;
        if endpoint.tls && tls.is_none() {
            return Err(Error::Invalid("an https endpoint needs a TLS configuration".into()));
        }
        let http = Http::new(endpoint, connector, tls, cfg.deadline);
        Ok(S3Client { http, cfg, creds, offset: AtomicI64::new(0), permits: Arc::new(tokio::sync::Semaphore::new(DEFAULT_MAX_CONCURRENT)) })
    }

    /// Allow at most `n` (at least 1) requests in flight at once.
    pub fn with_max_concurrent(mut self, n: usize) -> S3Client {
        self.permits = Arc::new(tokio::sync::Semaphore::new(n.max(1)));
        self
    }

    pub fn config(&self) -> &S3Config {
        &self.cfg
    }

    /// The server's clock as far as the last reply told (our own before any).
    pub fn server_now(&self) -> SystemTime {
        let secs = self.now_unix();
        UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap_or(0))
    }

    fn now_unix(&self) -> i64 {
        let local = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        local + self.offset.load(Ordering::Relaxed)
    }

    /// The name of an object: prefix and key.
    pub fn object_key(&self, key: &str) -> String {
        if self.cfg.prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{}/{key}", self.cfg.prefix)
        }
    }

    async fn call(&self, method: &str, key: Option<&str>, query: &[(&str, String)], extra: &[(&str, String)], body: &[u8], max_reply: usize) -> std::result::Result<Reply, S3Fail> {
        let endpoint_host = self.http.endpoint.host.clone();
        let (host, uri) = if self.cfg.path_style {
            (endpoint_host, match key {
                Some(k) => format!("/{}/{}", self.cfg.bucket, uri_encode(k, true)),
                None => format!("/{}", self.cfg.bucket),
            })
        } else {
            (format!("{}.{endpoint_host}", self.cfg.bucket), match key {
                Some(k) => format!("/{}", uri_encode(k, true)),
                None => "/".to_owned(),
            })
        };
        let mut q: Vec<(String, String)> = query.iter().map(|(n, v)| (uri_encode(n, false), uri_encode(v, false))).collect();
        q.sort();
        let canonical_query = q.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("&");

        let payload = sha256_hex(body);
        let (amz_date, _) = amz_dates(self.now_unix());
        let mut headers: Vec<(String, String)> = vec![
            ("host".into(), self.http.endpoint.host_header(&host)),
            ("x-amz-content-sha256".into(), payload.clone()),
            ("x-amz-date".into(), amz_date.clone()),
        ];
        headers.extend(extra.iter().map(|(n, v)| ((*n).to_owned(), v.trim().to_owned())));
        headers.sort_by(|a, b| a.0.cmp(&b.0));
        let auth = authorization(&Request {
            method,
            canonical_uri: &uri,
            canonical_query: &canonical_query,
            headers: &headers,
            payload_sha256: &payload,
            amz_date: &amz_date,
            region: &self.cfg.region,
            access_key: &self.creds.access_key,
            secret_key: &self.creds.secret_key,
        });
        // `Host` is added by the client itself; the other signed headers and the signature are sent.
        let mut sent: Vec<(String, String)> = headers.into_iter().filter(|(n, _)| n != "host").collect();
        sent.push(("authorization".into(), auth));
        let target = if canonical_query.is_empty() { uri } else { format!("{uri}?{canonical_query}") };
        let _permit = match tokio::time::timeout(self.cfg.deadline, self.permits.clone().acquire_owned()).await {
            Ok(Ok(p)) => p,
            _ => return Err(S3Fail::Timeout),
        };
        let reply = self.http.send(method, &target, &host, &sent, body, max_reply).await?;
        if let Some(date) = reply.header("date").and_then(parse_http_date) {
            let local = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
            self.offset.store(date - local, Ordering::Relaxed);
        }
        Ok(reply)
    }

    /// Store `body` under `key`, with the SHA-256 of the body signed: a body changed on the way is refused by the server.
    pub async fn put(&self, key: &str, body: Bytes) -> std::result::Result<(), S3Fail> {
        let reply = self.call("PUT", Some(&self.object_key(key)), &[], &[], &body, MAX_SMALL_REPLY).await?;
        if reply.status == 200 {
            Ok(())
        } else {
            Err(classify(&reply))
        }
    }

    /// The object, or `range` (`first`, `last` inclusive) of it. At most `max` bytes.
    pub async fn get(&self, key: &str, range: Option<(u64, u64)>, max: usize) -> std::result::Result<Bytes, S3Fail> {
        let extra: Vec<(&str, String)> = range.map(|(a, b)| vec![("range", format!("bytes={a}-{b}"))]).unwrap_or_default();
        let reply = self.call("GET", Some(&self.object_key(key)), &[], &extra, &[], max).await?;
        match reply.status {
            200 | 206 => Ok(reply.body),
            _ => Err(classify(&reply)),
        }
    }

    /// Size and modification time, or `None` when there is no such object.
    pub async fn head(&self, key: &str) -> std::result::Result<Option<ObjectMeta>, S3Fail> {
        let reply = self.call("HEAD", Some(&self.object_key(key)), &[], &[], &[], 0).await?;
        match reply.status {
            200 => {
                let size = reply.header("content-length").and_then(|v| v.parse().ok()).ok_or(S3Fail::Protocol)?;
                let modified_unix = reply.header("last-modified").and_then(parse_http_date).ok_or(S3Fail::Protocol)?;
                Ok(Some(ObjectMeta { size, modified_unix }))
            }
            404 => Ok(None),
            _ => Err(classify(&reply)),
        }
    }

    /// Make the bucket (for set-up tools and tests; a deployment is given a bucket that exists). Making one that is already yours is not an error.
    #[doc(hidden)]
    pub async fn create_bucket(&self) -> std::result::Result<(), S3Fail> {
        let reply = self.call("PUT", None, &[], &[], &[], MAX_SMALL_REPLY).await?;
        match reply.status {
            200 => Ok(()),
            409 if std::str::from_utf8(&reply.body).is_ok_and(|t| t.contains("BucketAlreadyOwnedByYou")) => Ok(()),
            _ => Err(classify(&reply)),
        }
    }

    /// Delete the object; deleting what is not there is not an error.
    pub async fn delete(&self, key: &str) -> std::result::Result<(), S3Fail> {
        let reply = self.call("DELETE", Some(&self.object_key(key)), &[], &[], &[], MAX_SMALL_REPLY).await?;
        match reply.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(classify(&reply)),
        }
    }

    /// One page of the objects whose key starts with `prefix` (relative to the configured prefix), after `token`.
    pub async fn list(&self, prefix: &str, token: Option<&str>) -> std::result::Result<ListPage, S3Fail> {
        let mut query = vec![("list-type", "2".to_owned()), ("max-keys", PAGE.to_string()), ("prefix", self.object_key(prefix))];
        if let Some(t) = token {
            query.push(("continuation-token", t.to_owned()));
        }
        let reply = self.call("GET", None, &query, &[], &[], MAX_SMALL_REPLY).await?;
        if reply.status != 200 {
            return Err(classify(&reply));
        }
        let text = std::str::from_utf8(&reply.body).map_err(|_| S3Fail::Protocol)?;
        let strip = if self.cfg.prefix.is_empty() { String::new() } else { format!("{}/", self.cfg.prefix) };
        let mut entries = Vec::new();
        for block in xml::blocks(text, "Contents") {
            let (Some(key), Some(size), Some(modified)) = (xml::text_of(block, "Key"), xml::text_of(block, "Size"), xml::text_of(block, "LastModified")) else {
                return Err(S3Fail::Protocol);
            };
            let Some(relative) = key.strip_prefix(&strip) else { continue };
            entries.push(ListEntry {
                key: relative.to_owned(),
                size: size.parse().map_err(|_| S3Fail::Protocol)?,
                modified_unix: parse_iso8601(&modified).ok_or(S3Fail::Protocol)?,
            });
        }
        let truncated = xml::text_of(text, "IsTruncated").is_some_and(|v| v == "true");
        let next = if truncated { Some(xml::text_of(text, "NextContinuationToken").ok_or(S3Fail::Protocol)?) } else { None };
        Ok(ListPage { entries, next })
    }
}

/// Sort an error reply into a category: by the S3 error code when there is one, by the status otherwise.
pub(crate) fn classify(reply: &Reply) -> S3Fail {
    let code = std::str::from_utf8(&reply.body).ok().and_then(|t| xml::text_of(t, "Code"));
    match code.as_deref() {
        Some("AccessDenied" | "AllAccessDisabled" | "AccountProblem") => return S3Fail::Denied,
        Some("InvalidAccessKeyId" | "SignatureDoesNotMatch" | "RequestTimeTooSkewed" | "ExpiredToken" | "InvalidToken" | "AuthorizationHeaderMalformed") => return S3Fail::Auth,
        Some("NoSuchBucket") => return S3Fail::NoBucket,
        Some("NoSuchKey") => return S3Fail::NotFound,
        Some("SlowDown" | "RequestLimitExceeded" | "Throttling" | "ThrottlingException" | "ServiceUnavailable") => return S3Fail::Throttled,
        _ => {}
    }
    match reply.status {
        401 => S3Fail::Auth,
        403 => S3Fail::Denied,
        404 => S3Fail::NotFound,
        429 | 503 => S3Fail::Throttled,
        500..=599 => S3Fail::Server,
        _ => S3Fail::Protocol,
    }
}

/// The TLS configuration for the connections: the web roots of the system, or `ca_pem` (a CA bundle; a platform store whose server has a private CA). TLS 1.2 or 1.3, certificate and
/// name always verified.
pub fn tls_config(ca_pem: Option<&[u8]>) -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    match ca_pem {
        Some(pem) => {
            let certs = rustls_pemfile_certs(pem)?;
            if certs.is_empty() {
                return Err(Error::Invalid("the CA bundle holds no certificate".into()));
            }
            for c in certs {
                roots.add(c).map_err(|_| Error::Invalid("the CA bundle holds a certificate that cannot be used".into()))?;
            }
        }
        None => {
            let native = rustls_native_certs::load_native_certs();
            for c in native.certs {
                let _ = roots.add(c);
            }
            if roots.is_empty() {
                return Err(Error::storage("no trusted CA certificates on this system"));
            }
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| Error::storage("TLS protocol versions"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// PEM certificates out of `pem` (the `-----BEGIN CERTIFICATE-----` blocks), without a PEM library: the body of each block is base64 of the DER.
fn rustls_pemfile_certs(pem: &[u8]) -> Result<Vec<rustls_pki_types::CertificateDer<'static>>> {
    let text = std::str::from_utf8(pem).map_err(|_| Error::Invalid("the CA bundle is not text".into()))?;
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(s) = rest.find("-----BEGIN CERTIFICATE-----") {
        let body = &rest[s + "-----BEGIN CERTIFICATE-----".len()..];
        let Some(e) = body.find("-----END CERTIFICATE-----") else { return Err(Error::Invalid("the CA bundle has an unfinished certificate".into())) };
        let b64: String = body[..e].chars().filter(|c| !c.is_whitespace()).collect();
        out.push(rustls_pki_types::CertificateDer::from(base64_decode(&b64).ok_or_else(|| Error::Invalid("the CA bundle has an invalid certificate".into()))?));
        rest = &body[e + "-----END CERTIFICATE-----".len()..];
    }
    Ok(out)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> S3Config {
        S3Config { endpoint: "https://s3.example.com".into(), region: "us-east-1".into(), bucket: "rusty-mail".into(), prefix: "tenant/a".into(), path_style: true, deadline: Duration::from_secs(5) }
    }

    #[test]
    fn a_configuration_cannot_smuggle_a_path_a_host_or_a_header() {
        assert!(cfg().validate().is_ok());
        let with = |f: &dyn Fn(&mut S3Config)| {
            let mut c = cfg();
            f(&mut c);
            c.validate().is_err()
        };
        assert!(with(&|c| c.endpoint = "https://h/x".into()));
        assert!(with(&|c| c.endpoint = "ftp://h".into()));
        assert!(with(&|c| c.region = "US East".into()));
        assert!(with(&|c| c.bucket = "UPPER".into()));
        assert!(with(&|c| c.bucket = "ab".into()));
        assert!(with(&|c| c.bucket = "a..b".into()));
        assert!(with(&|c| c.bucket = "-abc".into()));
        for p in ["/x", "x/", "a//b", "../x", "a/../b", "a b", "a\r\nb", "."] {
            assert!(with(&|c| c.prefix = p.into()), "{p:?}");
        }
        assert!(!with(&|c| c.prefix = String::new()));
    }

    #[test]
    fn errors_are_sorted_by_code_then_by_status() {
        let r = |status: u16, body: &str| Reply { status, headers: vec![], body: Bytes::from(body.to_owned()) };
        let code = |c: &str| format!("<Error><Code>{c}</Code><Message>m</Message></Error>");
        assert_eq!(classify(&r(403, &code("AccessDenied"))), S3Fail::Denied);
        assert_eq!(classify(&r(403, &code("SignatureDoesNotMatch"))), S3Fail::Auth);
        assert_eq!(classify(&r(403, &code("InvalidAccessKeyId"))), S3Fail::Auth);
        assert_eq!(classify(&r(404, &code("NoSuchBucket"))), S3Fail::NoBucket);
        assert_eq!(classify(&r(404, &code("NoSuchKey"))), S3Fail::NotFound);
        assert_eq!(classify(&r(503, &code("SlowDown"))), S3Fail::Throttled);
        assert_eq!(classify(&r(500, "")), S3Fail::Server);
        assert_eq!(classify(&r(403, "")), S3Fail::Denied);
        assert_eq!(classify(&r(401, "")), S3Fail::Auth);
        assert_eq!(classify(&r(404, "")), S3Fail::NotFound);
        assert_eq!(classify(&r(418, "")), S3Fail::Protocol);
        assert!(S3Fail::Throttled.transient() && S3Fail::Connect.transient() && !S3Fail::Auth.transient() && !S3Fail::NoBucket.transient());
    }

    #[test]
    fn credentials_never_print_their_secret() {
        let c = Credentials { access_key: "AK".into(), secret_key: "very-secret".into() };
        assert!(!format!("{c:?}").contains("very-secret"));
    }

    #[test]
    fn a_ca_bundle_is_read_without_a_pem_library() {
        // The base64 helper against known vectors.
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8h").unwrap(), b"hello!");
        assert!(base64_decode("a*b").is_none());
        assert!(tls_config(Some(b"not a certificate")).is_err());
        assert!(tls_config(Some(b"-----BEGIN CERTIFICATE-----\nAAAA")).is_err());
    }
}
