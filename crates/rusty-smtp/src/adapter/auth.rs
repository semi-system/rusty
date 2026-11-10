//! The ONLY place `mail-auth` (DKIM signing and verification, SPF, DMARC) is named, together with
//! `adapter::dns` for the resolver it wraps. Everything that crosses this boundary is a Rusty-owned
//! type from `crate::dkim` or `crate::authres`; the library's types stay private to the adapter
//! (`pub(crate)` only, which `scripts/check-wrapped-libs.sh` enforces).
//!
//! Crypto backend: the library's pure-Rust `rust-crypto` feature (RustCrypto `rsa`, `ed25519-dalek`,
//! `sha2`); its default `aws-lc-rs` backend is a second C crypto stack and is not used. TLS is a
//! separate matter (rustls with ring, see `outbound::tls`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use mail_auth::crypto::{Ed25519Key, RsaKey, Sha256};
use mail_auth::headers::HeaderWriter;
use mail_auth::dkim::generate::DkimKeyPair;
use mail_auth::dkim::DkimSigner as LibSigner;
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
const ED_PEM_LABEL: &str = "PRIVATE KEY";

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

struct KeyMaterial {
    private: Vec<u8>,
    public: Vec<u8>,
}

/// Write the new key (mode 0600, never overwriting) and return what DNS needs.
pub(crate) fn generate(dir: &std::path::Path, domain: &str, selector: &str, algo: DkimAlgo) -> Result<GeneratedKey> {
    use base64::Engine;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let pair = match algo {
        DkimAlgo::Rsa2048 => {
            let (private, public) = crate::rsagen::generate_2048()?;
            // ring (inside the DKIM library) validates the key and its consistency; a signature made
            // with it must verify with the public key before the key is accepted.
            let pem = pem_armor(RSA_PEM_LABEL, &private);
            let loaded = load_rsa(pem.as_bytes(), "check.invalid", "check")?;
            let probe = b"From: a@check.invalid

x
";
            let sig = loaded.sign(probe)?;
            if !sig.contains("b=") {
                return Err(Error::Invalid("the generated RSA key failed its self-test".into()));
            }
            KeyMaterial { private, public }
        }
        DkimAlgo::Ed25519 => {
            let pair = DkimKeyPair::generate_ed25519().map_err(lib_err)?;
            KeyMaterial { private: pair.private_key().to_vec(), public: pair.public_key().to_vec() }
        }
    };
    let (label, suffix) = match algo {
        DkimAlgo::Rsa2048 => (RSA_PEM_LABEL, ""),
        DkimAlgo::Ed25519 => (ED_PEM_LABEL, "-ed25519"),
    };
    let public = match algo {
        DkimAlgo::Rsa2048 => rsa_spki(&pair.public),
        DkimAlgo::Ed25519 => pair.public.clone(),
    };
    let path = dir.join(format!("{domain}.{selector}{suffix}.pem"));
    let pem = pem_armor(label, &pair.private);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(0o400000) // O_NOFOLLOW on Linux
        .open(&path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => Error::Conflict("a DKIM key already exists for this domain and selector".into()),
            _ => Error::storage_with("cannot create the DKIM key file", e),
        })?;
    f.write_all(pem.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| Error::storage_with("cannot write the DKIM key file", e))?;
    Ok(GeneratedKey {
        algo,
        key_file: path,
        selector: format!("{selector}{suffix}"),
        public_key_b64: base64::engine::general_purpose::STANDARD.encode(public),
    })
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

/// Signs with `<dir>/<domain>.<selector>.pem` (RSA) and, when enabled, the ed25519 key beside it.
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

