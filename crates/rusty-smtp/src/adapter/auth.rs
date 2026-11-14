//! The ONLY place `mail-auth` (DKIM signing and verification, SPF, DMARC) is named, together with
//! `adapter::dns` for the resolver it wraps. Everything that crosses this boundary is a Rusty-owned
//! type from `crate::dkim` or `crate::authres`; the library's types stay private to the adapter
//! (`pub(crate)` only, which `scripts/check-wrapped-libs.sh` enforces).
//!
//! Crypto backend: the library's pure-Rust `rust-crypto` feature, patched (vendor/mail-auth/PATCHES.md) so
//! that its RSA operations run on `ring` instead of the RustCrypto `rsa` crate (RUSTSEC-2023-0071);
//! Ed25519 is `ed25519-dalek`. Its default `aws-lc-rs` backend is a second C crypto stack and is not used.
//! Key import checks keys with `ring` directly (the same library that signs them). TLS is a separate
//! matter (rustls with ring, see `outbound::tls`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use mail_auth::crypto::{Ed25519Key, RsaKey, Sha256};
use mail_auth::dkim::generate::DkimKeyPair;
use mail_auth::dkim::DkimSigner as LibSigner;
use mail_auth::headers::HeaderWriter;
use ring::signature::{self as ring_sig, KeyPair as _};
use rusty_core::{Error, Result};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::PrivateKeyDer;

use crate::dkim::{DkimAlgo, DkimSigner, GeneratedKey};

/// The header names a signature covers. Every name is listed twice: the second entry covers an
/// absent (or second) instance, so a header added in transit breaks the signature (oversigning,
/// RFC 6376 section 5.4). `From` is mandatory.
const SIGNED: &[&str] = &[
    "From", "To", "Cc", "Subject", "Date", "Message-ID", "In-Reply-To", "References", "Reply-To", "Sender",
    "MIME-Version", "Content-Type", "Content-Transfer-Encoding", "List-Id", "List-Unsubscribe",
];

fn lib_err(_: mail_auth::Error) -> Error {
    // The library's text is not passed on: it can quote key material or message content.
    Error::Invalid("DKIM operation failed".into())
}

// ------------------------------------------------------------------ key files

const RSA_PEM_LABEL: &str = "RSA PRIVATE KEY";
const PKCS8_PEM_LABEL: &str = "PRIVATE KEY";
/// RSA moduli accepted for signing: at least 2048 bits (RFC 8301 recommends it; 1024 is breakable by a
/// well-funded attacker), at most 4096 (what ring signs with, and what still fits a DNS TXT record).
const RSA_MIN_BITS: usize = 2048;
const RSA_MAX_BITS: usize = 4096;
/// An imported PEM larger than this is not a single private key.
const MAX_IMPORT_PEM: usize = 64 * 1024;

fn pem_armor(label: &str, der: &[u8]) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

fn der_len(n: usize) -> Vec<u8> {
    if n < 0x80 {
        vec![n as u8]
    } else if n <= 0xff {
        vec![0x81, n as u8]
    } else {
        vec![0x82, (n >> 8) as u8, n as u8]
    }
}

/// `SubjectPublicKeyInfo` for an RSA `RSAPublicKey` (PKCS #1) DER: what a DKIM `p=` should hold.
fn rsa_spki(pkcs1_public: &[u8]) -> Vec<u8> {
    // AlgorithmIdentifier { rsaEncryption, NULL }
    const ALG: [u8; 15] = [0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00];
    let mut bit_string = vec![0x00];
    bit_string.extend_from_slice(pkcs1_public);
    let mut bits = vec![0x03];
    bits.extend(der_len(bit_string.len()));
    bits.extend(bit_string);
    let mut body = ALG.to_vec();
    body.extend(bits);
    let mut out = vec![0x30];
    out.extend(der_len(body.len()));
    out.extend(body);
    out
}

fn bad_key(why: &str) -> Error {
    Error::Invalid(format!("DKIM key refused: {why}"))
}

/// A private key that passed every check, ready to be written.
struct Checked {
    algo: DkimAlgo,
    /// The key file content (PEM, only the key itself).
    pem: String,
    /// The `p=` value (RSA: SubjectPublicKeyInfo DER; Ed25519: the raw 32-byte key).
    public: Vec<u8>,
}

const PROBE: &[u8] = b"rusty dkim key self-test";
const PROBE_MESSAGE: &[u8] = b"From: a@check.invalid\r\nSubject: probe\r\n\r\nx\r\n";

/// An RSA key (PKCS #1 or PKCS #8 DER): ring loads it (structure and consistency checks), the modulus is in
/// range, a signature made with it verifies with its public key, and the DKIM signer loads and uses it.
fn check_rsa(kp: ring_sig::RsaKeyPair, pem: String) -> Result<Checked> {
    let bits = kp.public().modulus_len() * 8;
    if !(RSA_MIN_BITS..=RSA_MAX_BITS).contains(&bits) {
        return Err(bad_key("an RSA key must have 2048 to 4096 bits"));
    }
    let rng = ring::rand::SystemRandom::new();
    let mut sig = vec![0u8; kp.public().modulus_len()];
    kp.sign(&ring_sig::RSA_PKCS1_SHA256, &rng, PROBE, &mut sig).map_err(|_| bad_key("the RSA key cannot sign"))?;
    let public = kp.public_key().as_ref().to_vec();
    ring_sig::UnparsedPublicKey::new(&ring_sig::RSA_PKCS1_2048_8192_SHA256, &public)
        .verify(PROBE, &sig)
        .map_err(|_| bad_key("the RSA key failed its sign-and-verify self-test"))?;
    load_rsa(pem.as_bytes(), "check.invalid", "check")?.sign(PROBE_MESSAGE)?;
    Ok(Checked { algo: DkimAlgo::Rsa, pem, public: rsa_spki(&public) })
}

/// An Ed25519 key (PKCS #8 v1 or v2 DER): the same checks.
fn check_ed25519(kp: ring_sig::Ed25519KeyPair, pem: String) -> Result<Checked> {
    let sig = kp.sign(PROBE);
    let public = kp.public_key().as_ref().to_vec();
    ring_sig::UnparsedPublicKey::new(&ring_sig::ED25519, &public)
        .verify(PROBE, sig.as_ref())
        .map_err(|_| bad_key("the ed25519 key failed its sign-and-verify self-test"))?;
    load_ed(pem.as_bytes(), "check.invalid", "check")?.sign(PROBE_MESSAGE)?;
    Ok(Checked { algo: DkimAlgo::Ed25519, pem, public })
}

/// Recognise and check a private key. Only the key itself is kept (other PEM sections are dropped).
fn check_key(der: PrivateKeyDer<'_>) -> Result<Checked> {
    match der {
        PrivateKeyDer::Pkcs1(k) => {
            let der = k.secret_pkcs1_der();
            let kp = ring_sig::RsaKeyPair::from_der(der).map_err(|_| bad_key("not a usable RSA key (2048 to 4096 bits)"))?;
            check_rsa(kp, pem_armor(RSA_PEM_LABEL, der))
        }
        PrivateKeyDer::Pkcs8(k) => {
            let der = k.secret_pkcs8_der();
            let pem = pem_armor(PKCS8_PEM_LABEL, der);
            if let Ok(kp) = ring_sig::RsaKeyPair::from_pkcs8(der) {
                return check_rsa(kp, pem);
            }
            match ring_sig::Ed25519KeyPair::from_pkcs8_maybe_unchecked(der) {
                Ok(kp) => check_ed25519(kp, pem),
                Err(_) => Err(bad_key("only RSA (2048 to 4096 bits) and Ed25519 keys can sign DKIM")),
            }
        }
        _ => Err(bad_key("only RSA (2048 to 4096 bits) and Ed25519 keys can sign DKIM")),
    }
}

/// Write the checked key under the keyring's name for it (mode 0600, never overwriting) and return what DNS
/// needs. `create_new` is `O_CREAT | O_EXCL`: an existing file or symlink at the name is never followed.
fn write_key(dir: &std::path::Path, domain: &str, selector: &str, key: Checked) -> Result<GeneratedKey> {
    use base64::Engine;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let suffix = match key.algo {
        DkimAlgo::Rsa => "",
        DkimAlgo::Ed25519 => "-ed25519",
    };
    let path = dir.join(format!("{domain}.{selector}{suffix}.pem"));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => Error::Conflict("a DKIM key already exists for this domain and selector".into()),
        _ => Error::storage_with("cannot create the DKIM key file", e),
    })?;
    if let Err(e) = f.write_all(key.pem.as_bytes()).and_then(|()| f.sync_all()) {
        let _ = std::fs::remove_file(&path);
        return Err(Error::storage_with("cannot write the DKIM key file", e));
    }
    Ok(GeneratedKey {
        algo: key.algo,
        key_file: path,
        selector: format!("{selector}{suffix}"),
        public_key_b64: base64::engine::general_purpose::STANDARD.encode(key.public),
    })
}


/// The key file `write_key` would have made for `(domain, selector, algo)`, read back and checked like an imported
/// key, as what DNS needs. `None` when there is no such file. Only a regular file is read (never a symlink), at most
/// [`MAX_IMPORT_PEM`] bytes. This is how a second `rustyctl dkim generate` shows the record of the key it will not
/// replace.
pub(crate) fn existing(dir: &std::path::Path, domain: &str, selector: &str, algo: DkimAlgo) -> Result<Option<GeneratedKey>> {
    use base64::Engine;
    use std::io::Read;
    let suffix = match algo {
        DkimAlgo::Rsa => "",
        DkimAlgo::Ed25519 => "-ed25519",
    };
    let path = dir.join(format!("{domain}.{selector}{suffix}.pem"));
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Err(Error::Invalid("the DKIM key path is not a regular file".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::storage_with("cannot inspect the DKIM key file", e)),
    }
    let mut pem = Vec::new();
    std::fs::File::open(&path)
        .and_then(|f| f.take(MAX_IMPORT_PEM as u64 + 1).read_to_end(&mut pem))
        .map_err(|e| Error::storage_with("cannot read the DKIM key file", e))?;
    if pem.len() > MAX_IMPORT_PEM {
        return Err(bad_key("the file is too large to be one private key"));
    }
    let der = PrivateKeyDer::from_pem_slice(&pem).map_err(|_| bad_key("no unencrypted PEM private key found"))?;
    let key = check_key(der)?;
    if key.algo != algo {
        return Err(bad_key("the key file holds a key of another algorithm"));
    }
    Ok(Some(GeneratedKey {
        algo,
        key_file: path,
        selector: format!("{selector}{suffix}"),
        public_key_b64: base64::engine::general_purpose::STANDARD.encode(key.public),
    }))
}

/// A new Ed25519 key, checked like an imported one, then written.
pub(crate) fn generate_ed25519(dir: &std::path::Path, domain: &str, selector: &str) -> Result<GeneratedKey> {
    let pair = DkimKeyPair::generate_ed25519().map_err(lib_err)?;
    let der = rustls_pki_types::PrivatePkcs8KeyDer::from(pair.private_key());
    let key = check_key(PrivateKeyDer::Pkcs8(der))?;
    write_key(dir, domain, selector, key)
}

/// An operator-supplied PEM private key, checked, then written.
pub(crate) fn import(dir: &std::path::Path, domain: &str, selector: &str, pem: &[u8]) -> Result<GeneratedKey> {
    if pem.len() > MAX_IMPORT_PEM {
        return Err(bad_key("the file is too large to be one private key"));
    }
    // An encrypted key, a certificate or anything else without an unencrypted private key block is refused.
    let der = PrivateKeyDer::from_pem_slice(pem).map_err(|_| bad_key("no unencrypted PEM private key found"))?;
    let key = check_key(der)?;
    write_key(dir, domain, selector, key)
}

enum Loaded {
    Rsa(LibSigner<RsaKey<Sha256>, mail_auth::signer::Ready>),
    Ed(LibSigner<Ed25519Key, mail_auth::signer::Ready>),
}

fn load_rsa(pem: &[u8], domain: &str, selector: &str) -> Result<Loaded> {
    let der = PrivateKeyDer::from_pem_slice(pem).map_err(|_| Error::Invalid("DKIM key file is not a PEM private key".into()))?;
    let key = RsaKey::<Sha256>::from_key_der(der).map_err(lib_err)?;
    Ok(Loaded::Rsa(
        LibSigner::from_key(key)
            .domain(domain)
            .selector(selector)
            .headers(SIGNED.iter().chain(SIGNED.iter()).copied())
            .header_canonicalization(mail_auth::dkim::Canonicalization::Relaxed)
            .body_canonicalization(mail_auth::dkim::Canonicalization::Relaxed),
    ))
}

fn load_ed(pem: &[u8], domain: &str, selector: &str) -> Result<Loaded> {
    let der = PrivateKeyDer::from_pem_slice(pem).map_err(|_| Error::Invalid("DKIM key file is not a PEM private key".into()))?;
    let PrivateKeyDer::Pkcs8(p8) = der else {
        return Err(Error::Invalid("an ed25519 DKIM key must be PKCS #8".into()));
    };
    let key = Ed25519Key::from_pkcs8_der(p8.secret_pkcs8_der()).map_err(lib_err)?;
    Ok(Loaded::Ed(
        LibSigner::from_key(key)
            .domain(domain)
            .selector(selector)
            .headers(SIGNED.iter().chain(SIGNED.iter()).copied())
            .header_canonicalization(mail_auth::dkim::Canonicalization::Relaxed)
            .body_canonicalization(mail_auth::dkim::Canonicalization::Relaxed),
    ))
}

impl Loaded {
    fn sign(&self, message: &[u8]) -> Result<String> {
        let sig = match self {
            Loaded::Rsa(s) => s.sign(message),
            Loaded::Ed(s) => s.sign(message),
        }
        .map_err(lib_err)?;
        Ok(sig.to_header())
    }
}

/// Where a key file comes from: the secret source enforces mode 0600 and ownership.
pub(crate) trait KeyFiles: Send + Sync {
    fn read(&self, path: &std::path::Path) -> Result<Option<Vec<u8>>>;
}

type Entry = (Instant, Vec<Arc<Loaded>>);

/// Signs with `<dir>/<domain>.<selector>.pem` (RSA) and `<dir>/<domain>.<selector>-ed25519.pem` (unless
/// ed25519 signing is switched off), each when its file exists.
pub(crate) struct Keyring {
    dir: std::path::PathBuf,
    selector: String,
    ed25519: bool,
    files: Arc<dyn KeyFiles>,
    cache: std::sync::Mutex<std::collections::HashMap<String, Entry>>,
}

const KEY_TTL: Duration = Duration::from_secs(60);

impl Keyring {
    pub(crate) fn new(dir: std::path::PathBuf, selector: String, ed25519: bool, files: Arc<dyn KeyFiles>) -> Keyring {
        Keyring { dir, selector, ed25519, files, cache: Default::default() }
    }

    fn signers(&self, domain: &str) -> Result<Vec<Arc<Loaded>>> {
        let now = Instant::now();
        if let Ok(c) = self.cache.lock() {
            if let Some((at, v)) = c.get(domain) {
                if now.duration_since(*at) < KEY_TTL {
                    return Ok(v.clone());
                }
            }
        }
        let mut out = Vec::new();
        let rsa = self.dir.join(format!("{domain}.{}.pem", self.selector));
        if let Some(pem) = self.files.read(&rsa)? {
            out.push(Arc::new(load_rsa(&pem, domain, &self.selector)?));
        }
        if self.ed25519 {
            let ed = self.dir.join(format!("{domain}.{}-ed25519.pem", self.selector));
            if let Some(pem) = self.files.read(&ed)? {
                out.push(Arc::new(load_ed(&pem, domain, &format!("{}-ed25519", self.selector))?));
            }
        }
        if let Ok(mut c) = self.cache.lock() {
            c.insert(domain.to_owned(), (now, out.clone()));
        }
        Ok(out)
    }
}

impl DkimSigner for Keyring {
    fn sign(&self, domain: &str, message: &[u8]) -> Result<Vec<u8>> {
        let mut out = String::new();
        for s in self.signers(domain)? {
            out.push_str(&s.sign(message)?);
        }
        Ok(out.into_bytes())
    }
}
