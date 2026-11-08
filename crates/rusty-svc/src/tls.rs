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
        Ok(TlsProvider { resolver, config: Arc::new(config) })
    }

    /// From `[tls]`: the configured pair, or the explicit dev certificate; anything else is an
    /// error (listeners that accept authentication must not start in plaintext).
    pub fn from_config(cfg: &TlsConfig, hostname: &str, secrets: Arc<dyn SecretSource>) -> Result<TlsProvider> {
        match (&cfg.cert_file, &cfg.key_file) {
            (Some(c), Some(k)) => {
                let r = FileCertResolver::load(c, k, secrets)?;
                TlsProvider::from_resolver(Arc::new(r))
            }
            (None, None) if cfg.dev_self_signed => Self::dev_self_signed(hostname),
            _ => Err(bad("TLS is not configured: set [tls] cert_file and key_file")),
        }
    }

    #[cfg(feature = "dev-cert")]
    fn dev_self_signed(hostname: &str) -> Result<TlsProvider> {
        let mut names: Vec<String> = vec![hostname.to_owned()];
        for extra in ["localhost", "127.0.0.1", "::1"] {
            if !names.iter().any(|n| n == extra) {
                names.push(extra.to_owned());
            }
        }
        let made = rcgen::generate_simple_self_signed(names)
            .map_err(|e| Error::storage_with("cannot generate the dev certificate", e))?;
        let chain = vec![made.cert.der().clone()];
        let key = PrivateKeyDer::try_from(made.signing_key.serialize_der())
            .map_err(|_| bad("dev key is not usable"))?;
        let signing = default_provider()
            .key_provider
            .load_private_key(key)
            .map_err(|_| bad("dev key is not usable"))?;
        tracing::warn!(
            "DEV ONLY: serving an in-memory self-signed certificate ([tls] dev_self_signed). \
             Clients must be told to trust it; never use this on the internet."
        );
        TlsProvider::from_resolver(Arc::new(StaticCertResolver::new(CertifiedKey::new(chain, signing))))
    }

    #[cfg(not(feature = "dev-cert"))]
    fn dev_self_signed(_hostname: &str) -> Result<TlsProvider> {
        Err(bad("dev_self_signed needs a build with the dev-cert feature"))
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
