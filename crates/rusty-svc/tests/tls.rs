//! TLS: the certificate/key loader (modes, fail closed), the provider, reload on SIGHUP's
//! behalf, and the SNI resolver seam.

mod common;

use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use rusty_core::config::TlsConfig;
use rusty_core::Error;
use rusty_svc::{
    certified_key, parse_cert_chain, parse_private_key, CertResolver, FileSecretSource, SecretSource,
    TlsProvider,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn secrets() -> Arc<dyn SecretSource> {
    Arc::new(FileSecretSource::new())
}

fn tls_cfg(cert: &std::path::Path, key: &std::path::Path) -> TlsConfig {
    TlsConfig {
        cert_file: Some(cert.to_path_buf()),
        key_file: Some(key.to_path_buf()),
        dev_self_signed: false,
    }
}

fn invalid<T: std::fmt::Debug>(r: rusty_core::Result<T>) -> String {
    match r {
        Err(Error::Invalid(m)) => m,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

/// Serve one TLS echo connection per accept on a loopback port, using `provider`.
async fn echo_server(provider: TlsProvider) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = l.accept().await else { return };
            let acceptor = provider.acceptor();
            tokio::spawn(async move {
                let Ok(mut s) = acceptor.accept(tcp).await else { return };
                let mut buf = [0u8; 64];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

// backend-neutral: no database involved, TLS only
#[tokio::test]
async fn handshake_uses_the_configured_certificate() {
    let d = tempfile::tempdir().unwrap();
    let (cert, key) = make_pems(&["mail.example.test"]);
    let c = write_file(d.path(), "cert.pem", cert.as_bytes(), 0o644);
    let k = write_file(d.path(), "key.pem", key.as_bytes(), 0o600);
    let p = TlsProvider::from_config(&tls_cfg(&c, &k), "mail.example.test", secrets()).unwrap();
    let addr = echo_server(p).await;
    let (mut s, leaf) = tls_connect(addr, "mail.example.test", &cert).await;
    assert_eq!(leaf, der_of(&cert));
    s.write_all(b"ping").await.unwrap();
    let mut b = [0u8; 4];
    s.read_exact(&mut b).await.unwrap();
    assert_eq!(&b, b"ping");
}

// backend-neutral: no database involved, TLS only
#[tokio::test]
async fn reload_gives_new_connections_the_new_certificate_and_old_ones_continue() {
    let d = tempfile::tempdir().unwrap();
    let (cert_a, key_a) = make_pems(&["mail.example.test"]);
    let (cert_b, key_b) = make_pems(&["mail.example.test"]);
    assert_ne!(der_of(&cert_a), der_of(&cert_b));
    let c = write_file(d.path(), "cert.pem", cert_a.as_bytes(), 0o644);
    let k = write_file(d.path(), "key.pem", key_a.as_bytes(), 0o600);
    let p = TlsProvider::from_config(&tls_cfg(&c, &k), "mail.example.test", secrets()).unwrap();
    let addr = echo_server(p.clone()).await;

    let (mut old, leaf_old) = tls_connect(addr, "mail.example.test", &cert_a).await;
    assert_eq!(leaf_old, der_of(&cert_a));

    replace_file(&c, cert_b.as_bytes(), 0o644);
    replace_file(&k, key_b.as_bytes(), 0o600);
    p.reload().unwrap();

    let (_new, leaf_new) = tls_connect(addr, "mail.example.test", &cert_b).await;
    assert_eq!(leaf_new, der_of(&cert_b), "a new connection must see the reloaded certificate");

    old.write_all(b"still").await.unwrap();
    let mut b = [0u8; 5];
    old.read_exact(&mut b).await.unwrap();
    assert_eq!(&b, b"still", "an established connection continues across a reload");
}

// backend-neutral: no database involved, TLS only
#[tokio::test]
async fn a_failed_reload_keeps_the_old_certificate() {
    let d = tempfile::tempdir().unwrap();
    let (cert_a, key_a) = make_pems(&["mail.example.test"]);
    let c = write_file(d.path(), "cert.pem", cert_a.as_bytes(), 0o644);
    let k = write_file(d.path(), "key.pem", key_a.as_bytes(), 0o600);
    let p = TlsProvider::from_config(&tls_cfg(&c, &k), "mail.example.test", secrets()).unwrap();
    let addr = echo_server(p.clone()).await;

    // garbage key, then a key that does not match the certificate, then a loosened mode
    replace_file(&k, b"not a key", 0o600);
    assert!(p.reload().is_err());
    let (_, other_key) = make_pems(&["other.example.test"]);
    replace_file(&k, other_key.as_bytes(), 0o600);
    assert!(p.reload().is_err(), "a key that does not match the certificate is refused");
    replace_file(&k, key_a.as_bytes(), 0o640);
    assert!(p.reload().is_err(), "a group-readable key is refused on reload too");

    let (_s, leaf) = tls_connect(addr, "mail.example.test", &cert_a).await;
    assert_eq!(leaf, der_of(&cert_a));
}

// backend-neutral: no database involved, file permission checks only
#[test]
fn key_file_modes_are_enforced_and_fail_closed() {
    let d = tempfile::tempdir().unwrap();
    let (cert, key) = make_pems(&["mail.example.test"]);
    let c = write_file(d.path(), "cert.pem", cert.as_bytes(), 0o644);

    for mode in [0o640, 0o604, 0o644, 0o660, 0o666, 0o602] {
        let k = write_file(d.path(), "key.pem", key.as_bytes(), mode);
        let m = invalid(TlsProvider::from_config(&tls_cfg(&c, &k), "h.example", secrets()));
        assert!(m.contains("0600"), "mode {mode:o}: {m}");
        assert!(!m.contains("PRIVATE"), "message must not carry key material: {m}");
    }
    // 0600 and 0400 are fine.
    for mode in [0o600, 0o400] {
        let k = write_file(d.path(), "key.pem", key.as_bytes(), mode);
        TlsProvider::from_config(&tls_cfg(&c, &k), "h.example", secrets()).unwrap();
        std::fs::set_permissions(&k, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // A cert file others can write is refused (they could swap the chain).
    let k = write_file(d.path(), "key.pem", key.as_bytes(), 0o600);
    let c_bad = write_file(d.path(), "cert-w.pem", cert.as_bytes(), 0o666);
    invalid(TlsProvider::from_config(&tls_cfg(&c_bad, &k), "h.example", secrets()));
}

// backend-neutral: no database involved, file type checks only
#[test]
fn key_file_must_be_a_regular_file_not_a_symlink_directory_or_huge() {
    let d = tempfile::tempdir().unwrap();
    let (cert, key) = make_pems(&["mail.example.test"]);
    let c = write_file(d.path(), "cert.pem", cert.as_bytes(), 0o644);
    let real = write_file(d.path(), "real.pem", key.as_bytes(), 0o600);
    let link = d.path().join("link.pem");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    invalid(TlsProvider::from_config(&tls_cfg(&c, &link), "h.example", secrets()));
    invalid(TlsProvider::from_config(&tls_cfg(&c, d.path()), "h.example", secrets()));
    let fifo = d.path().join("fifo.pem");
    rustix::fs::mknodat(rustix::fs::CWD, &fifo, rustix::fs::FileType::Fifo, rustix::fs::Mode::from_raw_mode(0o600), 0)
        .unwrap();
    invalid(TlsProvider::from_config(&tls_cfg(&c, &fifo), "h.example", secrets()));
    let huge = write_file(d.path(), "huge.pem", &vec![b'#'; 2 * 1024 * 1024], 0o600);
    invalid(TlsProvider::from_config(&tls_cfg(&c, &huge), "h.example", secrets()));
    // Missing file: an error, never a plaintext fallback.
    assert!(TlsProvider::from_config(&tls_cfg(&c, &d.path().join("nope.pem")), "h.example", secrets()).is_err());
}

// backend-neutral: no database involved, key/cert consistency only
#[test]
fn a_key_that_does_not_belong_to_the_certificate_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let (cert, _) = make_pems(&["a.example.test"]);
    let (_, other_key) = make_pems(&["b.example.test"]);
    let c = write_file(d.path(), "cert.pem", cert.as_bytes(), 0o644);
    let k = write_file(d.path(), "key.pem", other_key.as_bytes(), 0o600);
    invalid(TlsProvider::from_config(&tls_cfg(&c, &k), "h.example", secrets()));
}

// backend-neutral: no database involved, configuration choice only
#[test]
fn no_certificate_and_no_dev_flag_is_an_error_not_plaintext() {
    let cfg = TlsConfig::default();
    invalid(TlsProvider::from_config(&cfg, "h.example", secrets()));
}

// backend-neutral: no database involved, configuration choice only
#[cfg(feature = "dev-cert")]
#[tokio::test]
async fn dev_self_signed_serves_a_certificate_for_the_hostname_and_localhost() {
    let cfg = TlsConfig { cert_file: None, key_file: None, dev_self_signed: true };
    let p = TlsProvider::from_config(&cfg, "dev.example.test", secrets()).unwrap();
    let addr = echo_server(p.clone()).await;
    // Trust whatever the server presents: grab it through a verifier-less connect and then
    // verify that the same certificate validates for both names.
    let (_, leaf) = common::tls_connect_insecure(addr).await;

    let (_s, leaf2) = tls_connect_trusting_der(addr, "dev.example.test", &leaf).await;
    let (_s2, leaf3) = tls_connect_trusting_der(addr, "localhost", &leaf).await;
    assert_eq!(leaf, leaf2);
    assert_eq!(leaf, leaf3);
}

// backend-neutral: no database involved, configuration choice only
#[cfg(not(feature = "dev-cert"))]
#[test]
fn dev_self_signed_is_refused_in_a_build_without_the_feature() {
    let cfg = TlsConfig { cert_file: None, key_file: None, dev_self_signed: true };
    invalid(TlsProvider::from_config(&cfg, "dev.example.test", secrets()));
}

#[derive(Debug)]
struct CountingResolver {
    inner: Arc<dyn CertResolver>,
    calls: AtomicUsize,
    last_sni: std::sync::Mutex<Option<String>>,
}

impl CertResolver for CountingResolver {
    fn resolve(&self, sni: Option<&str>) -> Option<Arc<rustls::sign::CertifiedKey>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_sni.lock().unwrap() = sni.map(str::to_owned);
        self.inner.resolve(sni)
    }
}

// backend-neutral: no database involved, resolver seam only
#[tokio::test]
async fn certificates_are_resolved_through_the_sni_resolver_seam() {
    let (cert, key) = make_pems(&["mail.example.test"]);
    let ck = certified_key(cert.as_bytes(), key.as_bytes()).unwrap();
    let counting = Arc::new(CountingResolver {
        inner: Arc::new(rusty_svc::StaticCertResolver::new(ck)),
        calls: AtomicUsize::new(0),
        last_sni: std::sync::Mutex::new(None),
    });
    let p = TlsProvider::from_resolver(counting.clone()).unwrap();
    let addr = echo_server(p).await;
    let (_s, leaf) = tls_connect(addr, "mail.example.test", &cert).await;
    assert_eq!(leaf, der_of(&cert));
    assert_eq!(counting.calls.load(Ordering::SeqCst), 1);
    assert_eq!(counting.last_sni.lock().unwrap().as_deref(), Some("mail.example.test"));
}

// backend-neutral: no database involved, parser only
#[test]
fn pem_loader_rejects_what_it_cannot_use() {
    let (cert, key) = make_pems(&["mail.example.test"]);
    assert_eq!(parse_cert_chain(cert.as_bytes()).unwrap().len(), 1);
    parse_private_key(key.as_bytes()).unwrap();
    // empty, garbage, no certificate, a key where a certificate is expected, many certificates
    assert!(parse_cert_chain(b"").is_err());
    assert!(parse_cert_chain(b"hello").is_err());
    assert!(parse_cert_chain(key.as_bytes()).is_err());
    let eleven = cert.repeat(11);
    assert!(parse_cert_chain(eleven.as_bytes()).is_err());
    let ten = cert.repeat(10);
    assert_eq!(parse_cert_chain(ten.as_bytes()).unwrap().len(), 10);
    // no key, two keys, an encrypted key, a certificate where a key is expected
    assert!(parse_private_key(b"").is_err());
    assert!(parse_private_key(cert.as_bytes()).is_err());
    let two = format!("{key}{key}");
    assert!(parse_private_key(two.as_bytes()).is_err());
    let enc = "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n";
    assert!(parse_private_key(enc.as_bytes()).is_err());
    // bounded input
    assert!(parse_cert_chain(&vec![b'A'; 4 * 1024 * 1024]).is_err());
    let mut w = Vec::new();
    write!(w, "{cert}").unwrap();
    assert!(certified_key(&w, b"garbage").is_err());
}

// backend-neutral: no database involved, TLS only
#[tokio::test]
async fn an_unavailable_provider_says_so_and_never_completes_a_handshake() {
    let p = TlsProvider::unavailable().unwrap();
    assert!(!p.is_available());
    let addr = echo_server(p).await;
    let (cert, _) = make_pems(&["mail.example.test"]);
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let r = tokio_rustls::TlsConnector::from(client_config(&cert))
        .connect(rustls::pki_types::ServerName::try_from("mail.example.test".to_string()).unwrap(), tcp)
        .await;
    assert!(r.is_err(), "no certificate, no handshake");
    // a configured provider is available
    let (cert, key) = make_pems(&["mail.example.test"]);
    let ck = certified_key(cert.as_bytes(), key.as_bytes()).unwrap();
    assert!(TlsProvider::from_resolver(Arc::new(rusty_svc::StaticCertResolver::new(ck))).unwrap().is_available());
}
