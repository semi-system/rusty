//! Client-side TLS for outbound delivery: rustls with the `ring` provider (no second TLS stack, no
//! openssl). Two modes per connection:
//!
//! * strict (`verified` policy): the certificate chain and the host name are verified against the
//!   configured CA bundle (`[outbound] ca_bundle`) or, without one, the host's trust store; a failure
//!   is a failed handshake.
//! * permissive (`opportunistic` / `required`): the handshake is accepted whatever the certificate
//!   (the Internet's MTA-to-MTA reality), but the verification is still run and its result recorded, so
//!   the log can say "TLS 1.3, certificate not trusted" instead of nothing. The signature checks of the
//!   handshake itself are never skipped.
//!
//! The CA bundle path is an operator setting (see `OutboundConfig`); nothing a remote server or a
//! message says can change which roots are trusted.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rusty_core::{Error, Result};
use rusty_svc::BoxedStream;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{ring, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use tokio_rustls::TlsConnector;

/// What the handshake gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsInfo {
    /// The certificate chain and host name verified against the trust anchors.
    pub verified: bool,
    pub protocol: String,
}

#[async_trait]
pub trait TlsUpgrader: Send + Sync {
    /// Run the client handshake over `stream`. `strict`: fail unless the certificate verifies.
    async fn upgrade(&self, stream: BoxedStream, server_name: &str, strict: bool) -> std::result::Result<(BoxedStream, TlsInfo), String>;
}

const MAX_CA_BUNDLE: u64 = 1024 * 1024;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// Trust anchors from `bundle` (PEM, at most 1 MiB, at least one certificate) or the host's store.
pub fn load_roots(bundle: Option<&Path>) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    match bundle {
        Some(p) => {
            let meta = std::fs::metadata(p).map_err(|e| Error::storage_with("cannot read outbound.ca_bundle", e))?;
            if !meta.is_file() || meta.len() > MAX_CA_BUNDLE {
                return Err(Error::Invalid("outbound.ca_bundle must be a regular file of at most 1 MiB".into()));
            }
            let pem = std::fs::read(p).map_err(|e| Error::storage_with("cannot read outbound.ca_bundle", e))?;
            for cert in CertificateDer::pem_slice_iter(&pem) {
                let cert = cert.map_err(|_| Error::Invalid("outbound.ca_bundle is not valid PEM".into()))?;
                roots.add(cert).map_err(|_| Error::Invalid("outbound.ca_bundle holds an unusable certificate".into()))?;
            }
            if roots.is_empty() {
                return Err(Error::Invalid("outbound.ca_bundle holds no certificate".into()));
            }
        }
        None => {
            let found = rustls_native_certs::load_native_certs();
            for cert in found.certs {
                let _ = roots.add(cert);
            }
        }
    }
    Ok(roots)
}

/// Accepts every certificate but records whether it would have verified.
#[derive(Debug)]
struct Recording {
    inner: Option<Arc<WebPkiServerVerifier>>,
    provider: Arc<CryptoProvider>,
    verified: Arc<AtomicBool>,
}

impl ServerCertVerifier for Recording {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let ok = self
            .inner
            .as_ref()
            .is_some_and(|v| v.verify_server_cert(end_entity, intermediates, server_name, ocsp, now).is_ok());
        self.verified.store(ok, Ordering::Relaxed);
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

pub struct RustlsUpgrader {
    provider: Arc<CryptoProvider>,
    roots: Arc<RootCertStore>,
    strict: Arc<ClientConfig>,
}

impl RustlsUpgrader {
    /// `ca_bundle`: `[outbound] ca_bundle`. With no bundle and no usable system store, `verified`
    /// delivery fails every handshake (fail closed) while the permissive modes keep working.
    pub fn new(ca_bundle: Option<&Path>) -> Result<RustlsUpgrader> {
        let roots = Arc::new(load_roots(ca_bundle)?);
        let provider = provider();
        let strict = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|_| Error::Invalid("TLS protocol versions".into()))?
            .with_root_certificates((*roots).clone())
            .with_no_client_auth();
        Ok(RustlsUpgrader { provider, roots, strict: Arc::new(strict) })
    }
}

fn dns_name(host: &str) -> std::result::Result<ServerName<'static>, String> {
    ServerName::try_from(host.to_owned()).map_err(|_| "invalid server name for TLS".to_owned())
}

#[async_trait]
impl TlsUpgrader for RustlsUpgrader {
    async fn upgrade(&self, stream: BoxedStream, server_name: &str, strict: bool) -> std::result::Result<(BoxedStream, TlsInfo), String> {
        let name = dns_name(server_name)?;
        if strict {
            let tls = TlsConnector::from(self.strict.clone()).connect(name, stream).await.map_err(|e| format!("TLS handshake failed: {e}"))?;
            let protocol = protocol_of(tls.get_ref().1);
            return Ok((Box::new(tls), TlsInfo { verified: true, protocol }));
        }
        let verified = Arc::new(AtomicBool::new(false));
        let inner = if self.roots.is_empty() {
            None
        } else {
            WebPkiServerVerifier::builder_with_provider(self.roots.clone(), self.provider.clone()).build().ok()
        };
        let cfg = ClientConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|_| "TLS protocol versions".to_owned())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Recording { inner, provider: self.provider.clone(), verified: verified.clone() }))
            .with_no_client_auth();
        let tls = TlsConnector::from(Arc::new(cfg)).connect(name, stream).await.map_err(|e| format!("TLS handshake failed: {e}"))?;
        let protocol = protocol_of(tls.get_ref().1);
        Ok((Box::new(tls), TlsInfo { verified: verified.load(Ordering::Relaxed), protocol }))
    }
}

fn protocol_of(c: &rustls::ClientConnection) -> String {
    c.protocol_version().map_or_else(|| "unknown".to_owned(), |v| format!("{v:?}"))
}
