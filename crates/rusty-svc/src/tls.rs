//! TLS for the services: PEM loading with limits, certificate resolution through a seam
//! (`CertResolver`, keyed by SNI), and the provider that hands out acceptors.
//!
//! One certificate/key pair now (`FileCertResolver`, reloaded on SIGHUP); per-domain certificates
//! or an ACME daemon later are other `CertResolver` implementations. rustls with the `ring`
//! provider only; no openssl. Errors never carry key material.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use rusty_core::config::TlsConfig;
use rusty_core::{Error, Result};
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use crate::secret::{read_checked, SecretSource};

/// Most certificates accepted in one chain file (leaf plus intermediates).
pub const MAX_CERT_CHAIN: usize = 10;
/// Largest certificate file (a chain of ten is far below this).
const MAX_CERT_FILE_BYTES: u64 = 256 * 1024;
/// Largest PEM text the parsers look at.
const MAX_PEM_BYTES: usize = MAX_CERT_FILE_BYTES as usize;

fn bad(msg: &str) -> Error {
    Error::Invalid(msg.into())
}

/// Parse a PEM certificate chain: one to [`MAX_CERT_CHAIN`] certificates, leaf first. Bounded
/// input; never panics on any bytes (fuzzed).
pub fn parse_cert_chain(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    if pem.len() > MAX_PEM_BYTES {
        return Err(bad("certificate file is too large"));
    }
    let mut chain = Vec::new();
    for item in CertificateDer::pem_slice_iter(pem) {
        let cert = item.map_err(|_| bad("certificate file is not valid PEM"))?;
        if chain.len() == MAX_CERT_CHAIN {
            return Err(bad("certificate chain is too long"));
        }
        chain.push(cert);
    }
    if chain.is_empty() {
        return Err(bad("certificate file holds no certificate"));
    }
    Ok(chain)
}

/// Parse exactly one unencrypted PEM private key (PKCS#8, PKCS#1 or SEC1).
pub fn parse_private_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>> {
    if pem.len() > MAX_PEM_BYTES {
        return Err(bad("key file is too large"));
    }
    let mut keys = PrivateKeyDer::pem_slice_iter(pem);
    let first = match keys.next() {
        Some(Ok(k)) => k,
        Some(Err(_)) => return Err(bad("key file is not valid PEM")),
        None => return Err(bad("key file holds no usable private key (encrypted keys are not supported)")),
    };
    if keys.next().is_some() {
        return Err(bad("key file holds more than one key"));
    }
    Ok(first)
}

/// Build a certified key from PEM texts and check that key and certificate belong together.
pub fn certified_key(cert_pem: &[u8], key_pem: &[u8]) -> Result<CertifiedKey> {
    let chain = parse_cert_chain(cert_pem)?;
    let key = parse_private_key(key_pem)?;
    let signing = default_provider()
        .key_provider
        .load_private_key(key)
        .map_err(|_| bad("private key is not usable"))?;
    let ck = CertifiedKey::new(chain, signing);
    ck.keys_match().map_err(|_| bad("private key does not match the certificate"))?;
    Ok(ck)
}

/// Which certificate answers a handshake, keyed by the SNI name the client sent. A per-domain or
/// ACME-backed resolver replaces [`FileCertResolver`] without touching the services.
pub trait CertResolver: Send + Sync + fmt::Debug {
    fn resolve(&self, sni: Option<&str>) -> Option<Arc<CertifiedKey>>;
    /// Re-read the certificate material (SIGHUP). A failure must leave the old material in
    /// place. The default does nothing (static certificates).
    fn reload(&self) -> Result<()> {
        Ok(())
    }
}

/// One fixed certificate for every name.
#[derive(Debug)]
pub struct StaticCertResolver(Arc<CertifiedKey>);

impl StaticCertResolver {
    pub fn new(key: CertifiedKey) -> StaticCertResolver {
        StaticCertResolver(Arc::new(key))
    }
}

impl CertResolver for StaticCertResolver {
    fn resolve(&self, _sni: Option<&str>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

/// One certificate/key pair from files, replaced atomically by [`CertResolver::reload`]. The key
/// is read through a [`SecretSource`] (mode 0600, owned by the service account, no symlink); the
/// certificate file must at least not be writable by group or others.
pub struct FileCertResolver {
    cert_file: PathBuf,
    key_file: PathBuf,
    secrets: Arc<dyn SecretSource>,
    current: RwLock<Arc<CertifiedKey>>,
}

impl fmt::Debug for FileCertResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileCertResolver")
            .field("cert_file", &self.cert_file)
            .finish_non_exhaustive()
    }
}

impl FileCertResolver {
    fn read(cert_file: &Path, key_file: &Path, secrets: &dyn SecretSource) -> Result<CertifiedKey> {
        let cert = read_checked(cert_file, 0o022, MAX_CERT_FILE_BYTES, false, "certificate file must not be writable by group or others")?;
        let key = secrets.read(key_file)?;
        certified_key(&cert, &key)
    }

    /// Load and validate; any failure is an error (no plaintext fallback).
    pub fn load(cert_file: &Path, key_file: &Path, secrets: Arc<dyn SecretSource>) -> Result<FileCertResolver> {
        let ck = Self::read(cert_file, key_file, &*secrets)?;
        Ok(FileCertResolver {
            cert_file: cert_file.to_path_buf(),
            key_file: key_file.to_path_buf(),
            secrets,
            current: RwLock::new(Arc::new(ck)),
        })
    }
}

impl CertResolver for FileCertResolver {
    fn resolve(&self, _sni: Option<&str>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.read().unwrap_or_else(|e| e.into_inner()).clone())
    }

    fn reload(&self) -> Result<()> {
        let ck = Self::read(&self.cert_file, &self.key_file, &*self.secrets)?;
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(ck);
        Ok(())
    }
}

/// No certificate at all: every handshake fails. Used when only listeners that never accept
/// authentication run (port 25 then simply does not offer STARTTLS).
#[derive(Debug)]
struct NoCertResolver;

impl CertResolver for NoCertResolver {
    fn resolve(&self, _sni: Option<&str>) -> Option<Arc<CertifiedKey>> {
        None
    }
}

struct Adapter(Arc<dyn CertResolver>);

impl fmt::Debug for Adapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Adapter").field(&self.0).finish()
    }
}

impl ResolvesServerCert for Adapter {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.0.resolve(hello.server_name())
    }
}

/// Hands out TLS acceptors for every listener and for STARTTLS upgrades. Cloning is cheap and
/// shares the resolver, so one `reload()` reaches every service. Reload affects new handshakes
/// only; established connections keep their session keys.
#[derive(Clone)]
pub struct TlsProvider {
    resolver: Arc<dyn CertResolver>,
    config: Arc<ServerConfig>,
    available: bool,
}

impl fmt::Debug for TlsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsProvider").finish_non_exhaustive()
    }
}

impl TlsProvider {
    /// A provider over any resolver (TLS 1.2 and 1.3, ring).
    pub fn from_resolver(resolver: Arc<dyn CertResolver>) -> Result<TlsProvider> {
        let config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::storage_with("TLS protocol versions are not available", e))?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Adapter(resolver.clone())));
        Ok(TlsProvider { resolver, config: Arc::new(config), available: true })
    }

    /// [`from_config_in`](Self::from_config_in) without a data directory: the development certificate, if
    /// asked for, lives in memory only.
    pub fn from_config(cfg: &TlsConfig, hostname: &str, secrets: Arc<dyn SecretSource>) -> Result<TlsProvider> {
        Self::from_config_in(cfg, hostname, secrets, None)
    }

    /// From `[tls]`: the configured pair, or the explicit dev certificate; anything else is an
    /// error (listeners that accept authentication must not start in plaintext).
    /// `state_dir`: where the development certificate is kept between starts (0600 key; unsafe permissions
    /// are an error).
    pub fn from_config_in(cfg: &TlsConfig, hostname: &str, secrets: Arc<dyn SecretSource>, state_dir: Option<&Path>) -> Result<TlsProvider> {
        match (&cfg.cert_file, &cfg.key_file) {
            (Some(c), Some(k)) => {
                let r = FileCertResolver::load(c, k, secrets)?;
                TlsProvider::from_resolver(Arc::new(r))
            }
            (None, None) if cfg.dev_self_signed => Self::dev_self_signed(hostname, state_dir),
            _ => Err(bad("TLS is not configured: set [tls] cert_file and key_file")),
        }
    }

    #[cfg(feature = "dev-cert")]
    fn dev_self_signed(hostname: &str, state_dir: Option<&Path>) -> Result<TlsProvider> {
        let mut names: Vec<String> = vec![hostname.to_owned()];
        for extra in ["localhost", "127.0.0.1", "::1"] {
            if !names.iter().any(|n| n == extra) {
                names.push(extra.to_owned());
            }
        }
        let key = match state_dir {
            Some(dir) => dev_cert::load_or_create(dir, hostname, &names)?,
            None => dev_cert::make(hostname, &names)?,
        };
        let cert_key = certified_key(key.cert_pem.as_bytes(), key.key_pem.as_bytes())?;
        match state_dir {
            Some(dir) => tracing::warn!(
                dir = %dir.display(),
                "DEV ONLY: serving a self-signed certificate kept in the data dir ([tls] dev_self_signed). \
                 Clients must be told to trust it; never use this on the internet."
            ),
            None => tracing::warn!(
                "DEV ONLY: serving an in-memory self-signed certificate ([tls] dev_self_signed). \
                 Clients must be told to trust it; never use this on the internet."
            ),
        }
        TlsProvider::from_resolver(Arc::new(StaticCertResolver::new(cert_key)))
    }

    #[cfg(not(feature = "dev-cert"))]
    fn dev_self_signed(_hostname: &str, _state_dir: Option<&Path>) -> Result<TlsProvider> {
        Err(bad("dev_self_signed needs a build with the dev-cert feature"))
    }

    /// A provider without any certificate: [`is_available`](Self::is_available) is false and no
    /// handshake completes. Only for a start-up that runs no listener accepting authentication.
    pub fn unavailable() -> Result<TlsProvider> {
        let mut p = Self::from_resolver(Arc::new(NoCertResolver))?;
        p.available = false;
        Ok(p)
    }

    /// The leaf certificate currently served (DER), for tests and diagnostics.
    pub fn leaf_der(&self) -> Option<Vec<u8>> {
        self.resolver.resolve(None).and_then(|k| k.cert.first().map(|c| c.as_ref().to_vec()))
    }

    /// Sessions advertise STARTTLS only when this is true.
    pub fn is_available(&self) -> bool {
        self.available
    }

    pub fn acceptor(&self) -> TlsAcceptor {
        TlsAcceptor::from(self.config.clone())
    }

    /// The rustls server configuration (for an upgrade driver that works with rustls directly).
    pub fn server_config(&self) -> Arc<ServerConfig> {
        self.config.clone()
    }

    /// Re-read the certificate material. On error the previous certificate stays in service.
    pub fn reload(&self) -> Result<()> {
        self.resolver.reload()
    }
}

/// The development certificate: made once, kept in the data directory, replaced when the names change or it
/// is getting old. Only compiled with the `dev-cert` feature.
#[cfg(feature = "dev-cert")]
mod dev_cert {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use rusty_core::{Error, Result};

    use crate::secret::{read_checked, FileSecretSource, SecretSource};

    /// Validity of a new certificate and the age after which a kept one is replaced (a margin before expiry).
    const VALID_DAYS: u64 = 365;
    const REPLACE_AFTER: Duration = Duration::from_secs(330 * 86_400);
    const CERT_FILE: &str = "dev-tls-cert.pem";
    const KEY_FILE: &str = "dev-tls-key.pem";
    const NAMES_FILE: &str = "dev-tls-names.txt";

    pub(super) struct Pem {
        pub cert_pem: String,
        pub key_pem: String,
    }

    /// (year, month, day) of a day count since 1970-01-01 (civil-from-days).
    fn civil(days: i64) -> (i32, u8, u8) {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        (if m <= 2 { y + 1 } else { y } as i32, m, d)
    }

    /// A new certificate and key: subject CN = the host name (`localhost` for an address), the given SANs,
    /// valid from yesterday for a year.
    pub(super) fn make(hostname: &str, names: &[String]) -> Result<Pem> {
        let err = |e: rcgen::Error| Error::storage_with("cannot generate the dev certificate", e);
        let mut params = rcgen::CertificateParams::new(names.to_vec()).map_err(err)?;
        let cn = if hostname.parse::<std::net::IpAddr>().is_ok() || hostname.is_empty() { "localhost" } else { hostname };
        let mut dn = rcgen::DistinguishedName::new();
        dn.push(rcgen::DnType::CommonName, cn);
        params.distinguished_name = dn;
        let today = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() / 86_400)).unwrap_or(0);
        let (y1, m1, d1) = civil(today - 1);
        let (y2, m2, d2) = civil(today + i64::try_from(VALID_DAYS).unwrap_or(365));
        params.not_before = rcgen::date_time_ymd(y1, m1, d1);
        params.not_after = rcgen::date_time_ymd(y2, m2, d2);
        let key = rcgen::KeyPair::generate().map_err(err)?;
        let cert = params.self_signed(&key).map_err(err)?;
        Ok(Pem { cert_pem: cert.pem(), key_pem: key.serialize_pem() })
    }

    fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        let tmp = path.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            // create_new is O_CREAT | O_EXCL: a file or symlink planted at the name is never followed.
            .open(&tmp)
            .map_err(|e| Error::storage_with("cannot write the dev certificate", e))?;
        f.write_all(bytes).and_then(|()| f.sync_all()).map_err(|e| Error::storage_with("cannot write the dev certificate", e))?;
        std::fs::rename(&tmp, path).map_err(|e| Error::storage_with("cannot write the dev certificate", e))
    }

    fn fresh_enough(dir: &Path) -> bool {
        std::fs::metadata(dir.join(CERT_FILE))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age < REPLACE_AFTER)
    }

    /// The kept certificate when its files are sound and current; otherwise a new one is made and kept.
    /// Unsafe permissions on the kept key are an ERROR, never a silent regeneration (fail closed).
    pub(super) fn load_or_create(dir: &Path, hostname: &str, names: &[String]) -> Result<Pem> {
        let want = names.join("\n");
        let have_all = [CERT_FILE, KEY_FILE, NAMES_FILE].iter().all(|f| dir.join(f).exists());
        if have_all {
            let kept_names = std::fs::read_to_string(dir.join(NAMES_FILE)).unwrap_or_default();
            // Read through the checks first: a loosened key file stops the start instead of being replaced.
            let key = FileSecretSource::new().read(&dir.join(KEY_FILE))?;
            let cert = read_checked(&dir.join(CERT_FILE), 0o022, 256 * 1024, false, "certificate file must not be writable by group or others")?;
            if kept_names == want && fresh_enough(dir) {
                return Ok(Pem {
                    cert_pem: String::from_utf8(cert).map_err(|_| Error::Invalid("the dev certificate is not UTF-8".into()))?,
                    key_pem: String::from_utf8(key).map_err(|_| Error::Invalid("the dev key is not UTF-8".into()))?,
                });
            }
        }
        let pem = make(hostname, names)?;
        // Key first (0600), then the certificate, then the names marker: a crash in between leaves a set that
        // is regenerated next time, never a certificate without its key.
        write_new(&dir.join(KEY_FILE), pem.key_pem.as_bytes(), 0o600)?;
        write_new(&dir.join(CERT_FILE), pem.cert_pem.as_bytes(), 0o644)?;
        write_new(&dir.join(NAMES_FILE), want.as_bytes(), 0o644)?;
        let _ = std::fs::set_permissions(dir.join(KEY_FILE), std::fs::Permissions::from_mode(0o600));
        Ok(pem)
    }
}
