//! DKIM signing and inbound verification (log-only), end to end through the Rusty-owned seams:
//! ed25519 keys made by `generate_key`, RSA keys brought by the operator through `import_key`, signed
//! by the keyring, verified by the inbound checker on a pre-seeded resolver. The library behind them is
//! not named here.
//!
//! The PEM files under `tests/data/` are TEST-ONLY keys made with OpenSSL 3.5 (`openssl genpkey`), the
//! independent implementation the imported keys are checked against (`dkim-rsa2048.spki.b64` is
//! OpenSSL's own `-pubout` of `dkim-rsa2048.pem`); `dkim-ed25519-rfc8463.pem` is the RFC 8463 appendix A
//! example key. They sign nothing outside these tests.

use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use rusty_core::config::OutboundConfig;
use rusty_smtp::authres::AuthInput;
use rusty_smtp::dkim::{generate_key, import_key, keyring, DkimAlgo, DkimSigner, GeneratedKey};
use rusty_smtp::resolve::StaticDns;
use rusty_smtp::stack::DnsStack;
use rusty_svc::FileSecretSource;

const RSA_PKCS8: &[u8] = include_bytes!("data/dkim-rsa2048.pem");
const RSA_PKCS1: &[u8] = include_bytes!("data/dkim-rsa2048-pkcs1.pem");
const RSA_SPKI_B64: &str = include_str!("data/dkim-rsa2048.spki.b64");
const RSA_1024: &[u8] = include_bytes!("data/dkim-rsa1024.pem");
const RSA_ENCRYPTED: &[u8] = include_bytes!("data/dkim-rsa2048-encrypted.pem");
const EC_P256: &[u8] = include_bytes!("data/dkim-p256.pem");
const ED_RFC8463: &[u8] = include_bytes!("data/dkim-ed25519-rfc8463.pem");
/// RFC 8463 appendix A.2: the public key of the example ed25519 key.
const ED_RFC8463_PUBLIC: &str = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";

/// The operator's RSA key, imported under selector `rusty`.
fn rsa_key(dir: &std::path::Path, domain: &str) -> GeneratedKey {
    import_key(dir, domain, "rusty", RSA_PKCS8).unwrap()
}

const MSG: &[u8] = b"From: Alice <alice@example.org>\r\nTo: bob@remote.test\r\nSubject: hello\r\nDate: Mon, 09 Nov 2026 10:00:00 +0000\r\nMessage-ID: <1@example.org>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nHello Bob,\r\nsecond line\r\n";

fn cfg(dir: &std::path::Path, ed25519: bool) -> OutboundConfig {
    OutboundConfig { dkim_dir: Some(dir.to_owned()), dkim_ed25519: ed25519, ..OutboundConfig::default() }
}

fn dns_with(keys: &[&GeneratedKey]) -> StaticDns {
    let mut dns = StaticDns::default();
    for k in keys {
        dns.txt.insert(format!("{}._domainkey.example.org", k.selector), k.txt_value());
    }
    dns.txt.insert("_dmarc.example.org".into(), "v=DMARC1; p=none".into());
    dns.txt.insert("example.org".into(), "v=spf1 ip4:192.0.2.1 -all".into());
    dns
}

async fn check(stack: &DnsStack, ip: &str, message: &[u8]) -> rusty_smtp::authres::AuthResults {
    stack
        .checker
        .check(&AuthInput { peer: ip.parse().unwrap(), helo: "mail.example.org", mail_from: "alice@example.org", message })
        .await
}

fn signer(dir: &std::path::Path, ed: bool) -> Arc<dyn DkimSigner> {
    keyring(&cfg(dir, ed), Arc::new(FileSecretSource::new())).unwrap()
}

fn signed(sig: Vec<u8>) -> Vec<u8> {
    let mut v = sig;
    v.extend_from_slice(MSG);
    v
}

fn files_in(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).unwrap().count()
}

#[test]
fn an_imported_rsa_key_is_private_never_overwritten_and_publishes_its_own_public_key() {
    let d = tempfile::tempdir().unwrap();
    let k = rsa_key(d.path(), "example.org");
    assert_eq!(k.algo, DkimAlgo::Rsa);
    assert_eq!(std::fs::metadata(&k.key_file).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(k.key_file, d.path().join("example.org.rusty.pem"));
    assert_eq!(k.selector, "rusty");
    // The p= value is exactly what an independent implementation (OpenSSL) derives from the same key.
    assert_eq!(k.public_key_b64, RSA_SPKI_B64.trim());
    assert_eq!(k.txt_value(), format!("v=DKIM1; k=rsa; p={}", RSA_SPKI_B64.trim()));
    assert!(std::fs::read_to_string(&k.key_file).unwrap().starts_with("-----BEGIN PRIVATE KEY-----"));
    let again = import_key(d.path(), "example.org", "rusty", RSA_PKCS8);
    assert!(matches!(again, Err(rusty_core::Error::Conflict(_))), "{again:?}");
    // The traditional PKCS #1 encoding of the same key is accepted and gives the same record.
    let k1 = import_key(d.path(), "example.net", "rusty", RSA_PKCS1).unwrap();
    assert_eq!(k1.public_key_b64, RSA_SPKI_B64.trim());
    assert!(std::fs::read_to_string(&k1.key_file).unwrap().starts_with("-----BEGIN RSA PRIVATE KEY-----"));
    // File-name hostile input is refused before anything is written.
    let before = files_in(d.path());
    for bad in ["../x", "a/b", "", ".hidden", "a..b"] {
        assert!(import_key(d.path(), bad, "rusty", RSA_PKCS8).is_err(), "{bad}");
        assert!(import_key(d.path(), "example.com", bad, RSA_PKCS8).is_err(), "{bad}");
        assert!(generate_key(d.path(), bad, "rusty").is_err(), "{bad}");
        assert!(generate_key(d.path(), "example.com", bad).is_err(), "{bad}");
    }
    assert_eq!(files_in(d.path()), before);
}

#[test]
fn weak_unsupported_encrypted_or_garbage_keys_are_refused_and_nothing_is_written() {
    let d = tempfile::tempdir().unwrap();
    let cert = b"-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIU\n-----END CERTIFICATE-----\n";
    let cases: [(&str, &[u8]); 7] = [
        ("RSA-1024 is below the 2048-bit floor", RSA_1024),
        ("an EC key has no DKIM algorithm here", EC_P256),
        ("an encrypted key cannot be read", RSA_ENCRYPTED),
        ("a certificate is not a key", cert),
        ("empty", b""),
        ("not PEM", b"hello"),
        ("truncated", &RSA_PKCS8[..RSA_PKCS8.len() / 2]),
    ];
    for (why, pem) in cases {
        let r = import_key(d.path(), "example.org", "rusty", pem);
        assert!(matches!(r, Err(rusty_core::Error::Invalid(_))), "{why}: {r:?}");
    }
    assert_eq!(files_in(d.path()), 0, "a refused key leaves no file behind");
}

#[test]
fn generated_keys_are_ed25519_private_unique_and_never_overwritten() {
    let d = tempfile::tempdir().unwrap();
    let k = generate_key(d.path(), "example.org", "rusty").unwrap();
    assert_eq!(k.algo, DkimAlgo::Ed25519);
    assert_eq!(k.selector, "rusty-ed25519");
    assert_eq!(k.key_file, d.path().join("example.org.rusty-ed25519.pem"));
    assert_eq!(std::fs::metadata(&k.key_file).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(k.txt_value().starts_with("v=DKIM1; k=ed25519; p="), "{}", k.txt_value());
    assert!(matches!(generate_key(d.path(), "example.org", "rusty"), Err(rusty_core::Error::Conflict(_))));
    let k2 = generate_key(d.path(), "example.net", "rusty").unwrap();
    assert_ne!(k.public_key_b64, k2.public_key_b64, "two keys are never the same");
}

#[test]
fn the_rfc8463_example_key_imports_to_its_published_public_key() {
    let d = tempfile::tempdir().unwrap();
    let k = import_key(d.path(), "football.example.com", "brisbane", ED_RFC8463).unwrap();
    assert_eq!(k.algo, DkimAlgo::Ed25519);
    assert_eq!(k.public_key_b64, ED_RFC8463_PUBLIC);
    assert_eq!(k.selector, "brisbane-ed25519");
    assert_eq!(k.txt_value(), format!("v=DKIM1; k=ed25519; p={ED_RFC8463_PUBLIC}"));
}

#[tokio::test]
async fn a_signature_verifies_and_is_relaxed_rsa_with_oversigned_headers() {
    let d = tempfile::tempdir().unwrap();
    let k = rsa_key(d.path(), "example.org");
    let sig = signer(d.path(), false).sign("example.org", MSG).unwrap();
    let text = String::from_utf8(sig.clone()).unwrap();
    assert!(text.starts_with("DKIM-Signature: "), "{text}");
    let flat = text.replace("\r\n\t", "").replace("\r\n ", "").replace(' ', "");
    assert!(flat.contains("d=example.org;"), "{flat}");
    assert!(flat.contains("s=rusty;"), "{flat}");
    assert!(flat.contains("c=relaxed/relaxed;"), "{flat}");
    assert!(flat.contains("a=rsa-sha256;"), "{flat}");
    let h = flat.split("h=").nth(1).unwrap().split(';').next().unwrap();
    assert!(h.to_ascii_lowercase().matches("from").count() >= 2, "From is listed twice (oversigned): {h}");
    assert!(h.to_ascii_lowercase().contains("subject") && h.to_ascii_lowercase().contains("message-id"), "{h}");

    let stack = DnsStack::with_static(dns_with(&[&k]), "mx.local.test").unwrap();
    let r = check(&stack, "192.0.2.1", &signed(sig)).await;
    assert_eq!((r.spf.as_str(), r.dkim.as_str(), r.dmarc.as_str()), ("pass", "pass", "pass"), "{r:?}");
    assert!(r.header_value.starts_with("mx.local.test;"), "{}", r.header_value);
    assert!(!r.header_value.contains('\n'));
}

#[tokio::test]
async fn tampering_with_the_body_or_adding_a_from_header_breaks_the_signature() {
    let d = tempfile::tempdir().unwrap();
    let k = rsa_key(d.path(), "example.org");
    let sig = signer(d.path(), false).sign("example.org", MSG).unwrap();
    let stack = DnsStack::with_static(dns_with(&[&k]), "mx.local.test").unwrap();

    let mut body = signed(sig.clone());
    body.extend_from_slice(b"tampered\r\n");
    // The library reports a body-hash mismatch as neutral (RFC 8601 allows it); it must not pass.
    let r = check(&stack, "192.0.2.1", &body).await;
    assert!(r.dkim == "fail" || r.dkim == "neutral", "{r:?}");

    // A second From header in front of the signed one: oversigning makes the verifier include it.
    let mut forged = sig.clone();
    forged.extend_from_slice(b"From: ceo@example.org\r\n");
    forged.extend_from_slice(MSG);
    let r = check(&stack, "192.0.2.1", &forged).await;
    assert_eq!(r.dkim, "fail", "{r:?}");
}

#[tokio::test]
async fn spf_fails_from_a_foreign_address_and_there_is_no_dkim_without_a_signature() {
    let stack = DnsStack::with_static(dns_with(&[]), "mx.local.test").unwrap();
    let r = check(&stack, "203.0.113.9", MSG).await;
    assert_eq!((r.spf.as_str(), r.dkim.as_str()), ("fail", "none"), "{r:?}");
    assert_eq!(r.dmarc, "fail");
}

#[tokio::test]
async fn the_ed25519_second_signature_is_added_and_verifies() {
    let d = tempfile::tempdir().unwrap();
    let rsa = rsa_key(d.path(), "example.org");
    let ed = generate_key(d.path(), "example.org", "rusty").unwrap();
    assert_eq!(ed.selector, "rusty-ed25519");
    assert!(ed.txt_value().starts_with("v=DKIM1; k=ed25519; p="));
    // The ed25519 key is used unless it is switched off.
    assert!(OutboundConfig::default().dkim_ed25519, "an ed25519 key that exists is used by default");
    // Switched off, only the RSA signature is made.
    let one = String::from_utf8(signer(d.path(), false).sign("example.org", MSG).unwrap()).unwrap();
    assert_eq!(one.matches("DKIM-Signature:").count(), 1);
    let both = signer(d.path(), true).sign("example.org", MSG).unwrap();
    let text = String::from_utf8(both.clone()).unwrap();
    assert_eq!(text.matches("DKIM-Signature:").count(), 2, "{text}");
    assert!(text.contains("a=ed25519-sha256"));
    let stack = DnsStack::with_static(dns_with(&[&rsa, &ed]), "mx.local.test").unwrap();
    let r = check(&stack, "192.0.2.1", &signed(both)).await;
    assert_eq!(r.dkim, "pass", "{r:?}");
}

#[tokio::test]
async fn an_ed25519_key_alone_signs_and_verifies() {
    let d = tempfile::tempdir().unwrap();
    let ed = generate_key(d.path(), "example.org", "rusty").unwrap();
    let sig = signer(d.path(), true).sign("example.org", MSG).unwrap();
    let text = String::from_utf8(sig.clone()).unwrap();
    assert_eq!(text.matches("DKIM-Signature:").count(), 1, "{text}");
    assert!(text.contains("a=ed25519-sha256"), "{text}");
    let stack = DnsStack::with_static(dns_with(&[&ed]), "mx.local.test").unwrap();
    assert_eq!(check(&stack, "192.0.2.1", &signed(sig)).await.dkim, "pass");
}

#[test]
fn no_key_means_no_signature_and_a_loose_key_file_stops_signing() {
    let d = tempfile::tempdir().unwrap();
    assert!(signer(d.path(), false).sign("example.org", MSG).unwrap().is_empty());
    let k = rsa_key(d.path(), "example.org");
    assert!(!signer(d.path(), false).sign("example.org", MSG).unwrap().is_empty());
    std::fs::set_permissions(&k.key_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    // A fresh keyring (the first one caches for a minute) refuses the group/world-readable key.
    assert!(signer(d.path(), false).sign("example.org", MSG).is_err());
    assert!(keyring(&OutboundConfig::default(), Arc::new(FileSecretSource::new())).is_none(), "signing is off without dkim_dir");
}

#[test]
fn the_dns_generator_record_matches_the_generated_key() {
    // The record `rustyctl dkim generate` prints is the one the core generator publishes.
    let d = tempfile::tempdir().unwrap();
    let k = rsa_key(d.path(), "example.org");
    let ip =[IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))];
    let recs = rusty_core::dns::records_for_domain(&rusty_core::dns::DnsInputs {
        domain: "example.org",
        mail_host: "mail.example.org",
        outbound_ips: &ip,
        dkim: Some(rusty_core::dns::DkimPublic { selector: &k.selector, public_key_b64: &k.public_key_b64 }),
        dmarc_rua: None,
    })
    .unwrap();
    assert!(recs.iter().any(|r| r.value == k.txt_value()), "{recs:?}");
}

#[tokio::test]
async fn a_message_without_a_trailing_blank_is_still_checked() {
    let mut dns = StaticDns::default();
    dns.txt.insert("sender.example".into(), "v=spf1 ip4:127.0.0.1 -all".into());
    let stack = DnsStack::with_static(dns, "mx.local.test").unwrap();
    let msg = b"From: x@sender.example\r\nTo: alice@example.net\r\nSubject: spf\r\n\r\nbody\r\n";
    let r = stack
        .checker
        .check(&AuthInput { peer: "127.0.0.1".parse().unwrap(), helo: "client.example.org", mail_from: "x@sender.example", message: msg })
        .await;
    assert_eq!(r.spf, "pass", "{r:?}");
}
