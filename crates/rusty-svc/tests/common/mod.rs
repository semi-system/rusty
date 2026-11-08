#![allow(dead_code)]
//! Shared helpers of the rusty-svc tests: fixture certificates, secret files with a given
//! mode, a rustls client, a SQLite-backed `Context`.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

/// A fresh self-signed certificate for `names`: (cert PEM, key PEM).
pub fn make_pems(names: &[&str]) -> (String, String) {
    let ck = rcgen::generate_simple_self_signed(names.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .expect("fixture cert");
    (ck.cert.pem(), ck.signing_key.serialize_pem())
}

/// Write `bytes` to `dir/name` with exactly `mode`; returns the path.
pub fn write_file(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    p
}

/// Replace a file atomically (write a sibling, rename over it), keeping `mode`.
pub fn replace_file(path: &Path, bytes: &[u8], mode: u32) {
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, bytes).unwrap();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

pub fn client_config(trust_pem: &str) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for c in rustls_pemfile_like(trust_pem) {
        roots.add(c).unwrap();
    }
    let cfg = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

fn rustls_pemfile_like(pem: &str) -> Vec<CertificateDer<'static>> {
    use rustls::pki_types::pem::PemObject;
    CertificateDer::pem_slice_iter(pem.as_bytes()).map(|r| r.unwrap()).collect()
}

/// TLS connect to `addr` validating against `trust_pem` with server name `name`; returns the
/// stream and the DER of the leaf certificate the server presented.
pub async fn tls_connect(addr: SocketAddr, name: &str, trust_pem: &str) -> (TlsStream<TcpStream>, Vec<u8>) {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let connector = TlsConnector::from(client_config(trust_pem));
    let stream = connector
        .connect(ServerName::try_from(name.to_string()).unwrap(), tcp)
        .await
        .expect("tls handshake");
    let leaf = stream.get_ref().1.peer_certificates().unwrap()[0].as_ref().to_vec();
    (stream, leaf)
}

pub fn der_of(pem: &str) -> Vec<u8> {
    rustls_pemfile_like(pem)[0].as_ref().to_vec()
}

#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Connect without validating anything (to learn which certificate a server presents).
pub async fn tls_connect_insecure(addr: SocketAddr) -> (TlsStream<TcpStream>, Vec<u8>) {
    let cfg = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    let tcp = TcpStream::connect(addr).await.unwrap();
    let stream = TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from("whatever.invalid".to_string()).unwrap(), tcp)
        .await
        .expect("tls handshake");
    let leaf = stream.get_ref().1.peer_certificates().unwrap()[0].as_ref().to_vec();
    (stream, leaf)
}

/// Like `tls_connect`, trusting exactly the certificate whose DER is given.
pub async fn tls_connect_trusting_der(addr: SocketAddr, name: &str, der: &[u8]) -> (TlsStream<TcpStream>, Vec<u8>) {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(der.to_vec())).unwrap();
    let cfg = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tcp = TcpStream::connect(addr).await.unwrap();
    let stream = TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from(name.to_string()).unwrap(), tcp)
        .await
        .expect("tls handshake");
    let leaf = stream.get_ref().1.peer_certificates().unwrap()[0].as_ref().to_vec();
    (stream, leaf)
}

use rusty_core::config::StartupConfig;
use rusty_store::{BlobStore, FsBlobStore, Store, StoreConfig};

/// A SQLite store plus blob store in a temp dir (the composition-root way: `Store::open`).
pub struct Parts {
    pub store: Store,
    pub blobs: Arc<dyn BlobStore>,
    pub dir: tempfile::TempDir,
}

pub async fn parts() -> Parts {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open(StoreConfig::sqlite(format!("sqlite://{}", dir.path().join("meta.sqlite").display())))
        .await
        .unwrap();
    FsBlobStore::init(dir.path().join("blobs")).await.unwrap();
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::open(dir.path().join("blobs")).await.unwrap());
    Parts { store, blobs, dir }
}

/// Dev configuration under the parts' directory with every listener on an ephemeral port.
pub fn dev_config(dir: &Path) -> StartupConfig {
    use rusty_core::config::Listen;
    let mut c = StartupConfig::dev(dir);
    let any = Listen::on("127.0.0.1:0".parse().unwrap());
    c.smtp.listen_25 = any;
    c.smtp.listen_587 = any;
    c.smtp.listen_465 = any;
    c.imap.listen_143 = any;
    c.imap.listen_993 = any;
    c
}
