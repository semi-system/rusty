//! DKIM keys kept as sealed secrets (admin web design, section 14): the outbound service finds the sealed private key of a
//! domain in the database, opens it with its unsealing key, and signs. The public half of the key is in `domain_dkim_keys`,
//! the sealed private half in `sealed_secrets`; the private key never exists as a file on the machine that stored it.
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use rusty_core::config::OutboundConfig;
use rusty_seal::{generate, Context, Unsealer, UnsealingKey};
use rusty_smtp::authres::AuthInput;
use rusty_smtp::dkim::{generate_key, import_key, message_signer, sealed_purpose, GeneratedKey, MessageSigner, SealedKeyring};
use rusty_smtp::resolve::StaticDns;
use rusty_smtp::stack::DnsStack;
use rusty_store::{DnsStore, SealedReader, SealedWriter};
use rusty_svc::FileSecretSource;

const RSA_PKCS8: &[u8] = include_bytes!("data/dkim-rsa2048.pem");
const MSG: &[u8] = b"From: Alice <alice@example.org>\r\nTo: bob@remote.test\r\nSubject: hello\r\nDate: Mon, 09 Nov 2026 10:00:00 +0000\r\nMessage-ID: <1@example.org>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nHello Bob,\r\nsecond line\r\n";

struct Rig {
    b: Backend,
    unseal: Vec<UnsealingKey>,
}

impl Rig {
    fn dns(&self) -> DnsStore {
        DnsStore::with_repo(self.b.store.repos().dns.clone())
    }
    fn writer(&self) -> SealedWriter {
        SealedWriter::with_repo(self.b.store.repos().sealed.clone())
    }
    fn reader(&self) -> SealedReader {
        SealedReader::with_repo(self.b.store.repos().sealed.clone())
    }
}

/// An unsealing key (and a second one for rotation) and a keyring over this backend, with `ttl` for its cache.
fn keyring(b: &Backend, keys: Vec<UnsealingKey>, ttl: Duration) -> Arc<SealedKeyring> {
    let repos = b.store.repos();
    Arc::new(SealedKeyring::new(
        b.dir.clone(),
        DnsStore::with_repo(repos.dns.clone()),
        SealedReader::with_repo(repos.sealed.clone()),
        Unsealer::new(keys).unwrap(),
    )
    .with_ttl(ttl))
}

/// Make a key, keep only what the database should hold (the public row and the sealed private key) and return the record to publish.
async fn store_key(rig: &Rig, domain: &str, rsa: bool, seal_to: &rusty_seal::SealingKey) -> GeneratedKey {
    let tmp = tempfile::tempdir().unwrap();
    let k = if rsa { import_key(tmp.path(), domain, "rusty", RSA_PKCS8).unwrap() } else { generate_key(tmp.path(), domain, "rusty").unwrap() };
    let pem = std::fs::read(&k.key_file).unwrap();
    std::fs::remove_file(&k.key_file).unwrap();
    assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none(), "no key file is left behind");
    let dom = rig.b.dir.find_domain(domain).await.unwrap().unwrap();
    let kind = if rsa { "rsa" } else { "ed25519" };
    rig.dns().add_dkim_key(dom.id, &k.selector, kind, &k.public_key_b64).await.unwrap();
    let purpose = sealed_purpose(&k.selector);
    let sealed = seal_to.seal(&Context::new("domain", dom.id.0, &purpose).unwrap(), &pem).unwrap();
    rig.writer().put("domain", dom.id.0, &purpose, &seal_to.id().to_hex(), &sealed).await.unwrap();
    k
}

async fn verify(keys: &[&GeneratedKey], sig: Vec<u8>) -> rusty_smtp::authres::AuthResults {
    let mut dns = StaticDns::default();
    for k in keys {
        dns.txt.insert(format!("{}._domainkey.example.org", k.selector), k.txt_value());
    }
    dns.txt.insert("_dmarc.example.org".into(), "v=DMARC1; p=none".into());
    dns.txt.insert("example.org".into(), "v=spf1 ip4:192.0.2.1 -all".into());
    let stack = DnsStack::with_static(dns, "mail.example.org").unwrap();
    let mut message = sig;
    message.extend_from_slice(MSG);
    stack.checker.check(&AuthInput { peer: "192.0.2.1".parse().unwrap(), helo: "mail.example.org", mail_from: "alice@example.org", message: &message }).await
}

async fn rig_with_domain(name: &str) -> Option<(Rig, rusty_seal::SealingKey)> {
    let b = pg_backend().await?;
    b.dir.create_domain(name).await.unwrap();
    let (sk, pk) = generate();
    Some((Rig { b, unseal: vec![sk] }, pk))
}

macro_rules! rig {
    ($name:expr) => {
        match rig_with_domain($name).await {
            Some(x) => x,
            None => panic!("a Postgres test server (RUSTY_TEST_PG_URL)"),
        }
    };
}

fn take_key(rig: &Rig) -> UnsealingKey {
    // The unsealer owns its keys; a test that needs the same key twice makes it from the text form.
    UnsealingKey::from_text(rig.unseal[0].to_text().as_str()).unwrap()
}

// -------------------------------------------------------------------------------------------- signing

#[tokio::test]
async fn a_sealed_ed25519_key_signs_and_the_signature_verifies() {
    let (rig, pk) = rig!("example.org");
    let k = store_key(&rig, "example.org", false, &pk).await;
    let ring = keyring(&rig.b, vec![take_key(&rig)], Duration::from_secs(60));
    let sig = ring.sign("example.org", MSG).await.unwrap();
    assert!(String::from_utf8_lossy(&sig).starts_with("DKIM-Signature: "));
    let r = verify(&[&k], sig).await;
    assert_eq!(r.dkim, "pass", "{r:?}");
}

#[tokio::test]
async fn a_sealed_rsa_key_signs_too_and_both_keys_of_a_domain_sign_together() {
    let (rig, pk) = rig!("example.org");
    let ed = store_key(&rig, "example.org", false, &pk).await;
    let rsa = store_key(&rig, "example.org", true, &pk).await;
    assert_ne!(ed.selector, rsa.selector);
    let ring = keyring(&rig.b, vec![take_key(&rig)], Duration::from_secs(60));
    let sig = ring.sign("example.org", MSG).await.unwrap();
    let text = String::from_utf8_lossy(&sig).into_owned();
    assert_eq!(text.matches("DKIM-Signature: ").count(), 2, "{text}");
    assert_eq!(verify(&[&ed, &rsa], sig).await.dkim, "pass");
}

#[tokio::test]
async fn nothing_to_sign_with_is_an_empty_signature_not_an_error() {
    let (rig, pk) = rig!("example.org");
    let ring = keyring(&rig.b, vec![take_key(&rig)], Duration::from_secs(60));
    // A domain with no key, and a domain that does not exist.
    assert!(ring.sign("example.org", MSG).await.unwrap().is_empty());
    assert!(ring.sign("nope.test", MSG).await.unwrap().is_empty());
    // A key whose public half is published but whose private half was never sealed here (made elsewhere): not ours to sign with.
    let tmp = tempfile::tempdir().unwrap();
    let k = generate_key(tmp.path(), "example.org", "rusty").unwrap();
    let dom = rig.b.dir.find_domain("example.org").await.unwrap().unwrap();
    rig.dns().add_dkim_key(dom.id, &k.selector, "ed25519", &k.public_key_b64).await.unwrap();
    assert!(ring.sign("example.org", MSG).await.unwrap().is_empty());
    let _ = pk;
}

// ------------------------------------------------------------------------------------ what must refuse

#[tokio::test]
async fn a_key_that_cannot_be_opened_stops_signing_loudly_and_leaks_nothing() {
    let (rig, pk) = rig!("example.org");
    store_key(&rig, "example.org", false, &pk).await;
    let (stranger, _) = generate();
    let ring = keyring(&rig.b, vec![stranger], Duration::from_secs(60));
    let err = ring.sign("example.org", MSG).await.unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(!text.contains("PRIVATE KEY") && !text.contains("BEGIN"), "{text}");
    // Never silently unsigned: the caller defers the mail.
    let _ = take_key(&rig);
}

#[tokio::test]
async fn a_sealed_key_moved_to_another_domain_does_not_open_there() {
    let (rig, pk) = rig!("example.org");
    rig.b.dir.create_domain("other.test").await.unwrap();
    let k = store_key(&rig, "example.org", false, &pk).await;
    // Copy the sealed blob of example.org into other.test's row, with other.test's own public key row.
    let a = rig.b.dir.find_domain("example.org").await.unwrap().unwrap();
    let o = rig.b.dir.find_domain("other.test").await.unwrap().unwrap();
    let purpose = sealed_purpose(&k.selector);
    let blob = rig.reader().get("domain", a.id.0, &purpose).await.unwrap().unwrap();
    rig.dns().add_dkim_key(o.id, &k.selector, "ed25519", &k.public_key_b64).await.unwrap();
    rig.writer().put("domain", o.id.0, &purpose, &blob.key_id, &blob.sealed).await.unwrap();
    let ring = keyring(&rig.b, vec![take_key(&rig)], Duration::from_secs(60));
    assert!(ring.sign("example.org", MSG).await.is_ok());
    assert!(ring.sign("other.test", MSG).await.is_err(), "the blob is bound to the domain it was sealed for");
}

// ---------------------------------------------------------------------------- rotation and the cache

#[tokio::test]
async fn after_a_rotation_the_old_and_the_new_unsealing_key_both_work() {
    let (rig, old_pk) = rig!("example.org");
    let k = store_key(&rig, "example.org", false, &old_pk).await;
    let old = take_key(&rig);
    let (new_sk, _new_pk) = generate();
    let ring = keyring(&rig.b, vec![new_sk, old], Duration::from_secs(60));
    assert_eq!(verify(&[&k], ring.sign("example.org", MSG).await.unwrap()).await.dkim, "pass");
}

#[tokio::test]
async fn keys_are_cached_for_the_ttl_and_a_zero_ttl_sees_a_removal_at_once() {
    let (rig, pk) = rig!("example.org");
    let k = store_key(&rig, "example.org", false, &pk).await;
    let dom = rig.b.dir.find_domain("example.org").await.unwrap().unwrap();
    let cached = keyring(&rig.b, vec![take_key(&rig)], Duration::from_secs(300));
    let fresh = keyring(&rig.b, vec![take_key(&rig)], Duration::ZERO);
    assert!(!cached.sign("example.org", MSG).await.unwrap().is_empty());
    assert!(!fresh.sign("example.org", MSG).await.unwrap().is_empty());
    rig.writer().delete("domain", dom.id.0, &sealed_purpose(&k.selector)).await.unwrap();
    assert!(!cached.sign("example.org", MSG).await.unwrap().is_empty(), "within the ttl the loaded key is used");
    assert!(fresh.sign("example.org", MSG).await.unwrap().is_empty(), "no cache: the removal is seen");
}

// ------------------------------------------------------------------------ the chain with key files

#[tokio::test]
async fn sealed_keys_win_over_key_files_and_a_domain_with_only_a_file_still_signs() {
    let (rig, pk) = rig!("example.org");
    rig.b.dir.create_domain("fileonly.test").await.unwrap();
    let sealed_key = store_key(&rig, "example.org", false, &pk).await;
    let dir = tempfile::tempdir().unwrap();
    let file_key = generate_key(dir.path(), "example.org", "rusty").unwrap();
    let file_only = generate_key(dir.path(), "fileonly.test", "rusty").unwrap();
    assert_eq!(sealed_key.selector, file_key.selector, "the same selector in both places");
    let cfg = OutboundConfig { dkim_dir: Some(dir.path().to_owned()), ..OutboundConfig::default() };
    let files = rusty_smtp::dkim::keyring(&cfg, Arc::new(FileSecretSource::new())).unwrap();
    let ring = keyring(&rig.b, vec![take_key(&rig)], Duration::from_secs(60));
    let chain = rusty_smtp::dkim::chain(Some(ring), Some(files));
    let sig = chain.sign("example.org", MSG).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&sig).matches("DKIM-Signature: ").count(), 1, "one signature, not two with the same selector");
    // The sealed key is the one that signed: the published record of the sealed key verifies it (the file key is another key).
    assert_eq!(verify(&[&sealed_key], sig).await.dkim, "pass");
    // A domain that has only a key file keeps signing: nothing changes for installations that do not use sealed keys.
    let msg2 = b"From: A <a@fileonly.test>\r\nTo: b@remote.test\r\nSubject: s\r\nDate: Mon, 09 Nov 2026 10:00:00 +0000\r\nMessage-ID: <2@fileonly.test>\r\n\r\nx\r\n";
    let sig2 = chain.sign("fileonly.test", msg2).await.unwrap();
    assert!(String::from_utf8_lossy(&sig2).starts_with("DKIM-Signature: "));
    let _ = file_only;
}

#[tokio::test]
async fn a_file_keyring_is_a_message_signer_too() {
    let dir = tempfile::tempdir().unwrap();
    generate_key(dir.path(), "example.org", "rusty").unwrap();
    let cfg = OutboundConfig { dkim_dir: Some(dir.path().to_owned()), ..OutboundConfig::default() };
    let files = message_signer(rusty_smtp::dkim::keyring(&cfg, Arc::new(FileSecretSource::new())).unwrap());
    assert!(!files.sign("example.org", MSG).await.unwrap().is_empty());
}

#[test]
fn the_sealed_purpose_of_a_selector_fits_the_table_and_does_not_collide() {
    let a = sealed_purpose("rusty-ed25519");
    let b = sealed_purpose("Rusty-Ed25519");
    assert_eq!(a, b, "selectors are case-insensitive in DNS");
    assert_ne!(a, sealed_purpose("rusty"));
    assert!(a.starts_with("dkim-") && a.len() <= 32 && a.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'), "{a}");
}
