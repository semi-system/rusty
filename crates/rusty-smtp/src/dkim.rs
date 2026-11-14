//! DKIM signing as a Rusty-owned seam: `DkimSigner` (sign a message for a domain), ed25519 key
//! generation for `rustyctl dkim generate`, operator key import for `rustyctl dkim import`, and the public
//! record text. The crypto and the signature format are behind the DKIM library adapter.
//!
//! Policy (user decisions): an RSA signature (2048 bits or more) for the widest receiver compatibility and an
//! ed25519 signature (RFC 8463), each made when its key file exists. Rusty generates only ed25519 keys: the
//! RSA key is made by the operator with a standard tool (`openssl genpkey`) and imported, because no
//! acceptable RSA key generator is in the dependency tree (see [`generate_key`]). Relaxed/relaxed
//! canonicalisation; the signed header list includes
//! oversigning (every name twice) and always `From`; `d=` is the domain of the header `From`
//! (alignment), chosen by the caller, never the envelope domain or another tenant's.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusty_core::config::OutboundConfig;
use rusty_core::{Error, Result};
use rusty_svc::SecretSource;

use crate::adapter::auth as lib;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DkimAlgo {
    /// RSA (2048 to 4096 bits) with SHA-256; brought by the operator (`rustyctl dkim import`).
    Rsa,
    /// Ed25519 with SHA-256 (RFC 8463); made by `rustyctl dkim generate` or imported.
    Ed25519,
}

/// What `rustyctl dkim generate` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedKey {
    pub algo: DkimAlgo,
    pub key_file: PathBuf,
    /// The selector the TXT record is published under (the ed25519 key's has an `-ed25519` suffix).
    pub selector: String,
    /// Base64 `p=` value (RSA: SubjectPublicKeyInfo; ed25519: the raw key).
    pub public_key_b64: String,
}

impl GeneratedKey {
    /// The TXT value to publish at `<selector>._domainkey.<domain>`.
    pub fn txt_value(&self) -> String {
        match self.algo {
            DkimAlgo::Rsa => format!("v=DKIM1; k=rsa; p={}", self.public_key_b64),
            DkimAlgo::Ed25519 => format!("v=DKIM1; k=ed25519; p={}", self.public_key_b64),
        }
    }
}

/// Signs messages of a domain. `Ok(vec![])` when the domain has no key (the message goes unsigned);
/// otherwise the `DKIM-Signature` header lines (CRLF-terminated) to put in front of the message.
pub trait DkimSigner: Send + Sync {
    fn sign(&self, domain: &str, message: &[u8]) -> Result<Vec<u8>>;
}

/// A domain or selector fit to become part of a file name.
fn safe_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && !s.starts_with('.')
        && !s.contains("..")
}

fn check_labels(domain: &str, selector: &str) -> Result<()> {
    if !safe_label(domain) || !safe_label(selector) {
        return Err(Error::Invalid("domain or selector is not valid for a DKIM key file".into()));
    }
    Ok(())
}

/// Create an ed25519 key for `domain` in `dir` (mode 0600, never overwriting an existing key). Its selector
/// is `<selector>-ed25519`.
///
/// RSA keys are not generated here: `ring` (the crypto library of the tree) cannot make them, the RustCrypto
/// `rsa` crate (RUSTSEC-2023-0071) is not allowed, and hand-written prime generation is not an acceptable
/// substitute. The operator makes the RSA key with a standard tool and brings it with [`import_key`].
pub fn generate_key(dir: &Path, domain: &str, selector: &str) -> Result<GeneratedKey> {
    check_labels(domain, selector)?;
    lib::generate_ed25519(dir, domain, selector)
}


/// The key already kept under the name [`generate_key`] (`DkimAlgo::Ed25519`) or [`import_key`] (`DkimAlgo::Rsa`) would
/// write, as what DNS needs; `None` when there is none. Read back and checked like an imported key.
pub fn existing_key(dir: &Path, domain: &str, selector: &str, algo: DkimAlgo) -> Result<Option<GeneratedKey>> {
    check_labels(domain, selector)?;
    lib::existing(dir, domain, selector, algo)
}

/// Take an operator-supplied private key (PEM: RSA as PKCS #8 or PKCS #1, 2048 to 4096 bits; or Ed25519 as
/// PKCS #8), check it (it must load, sign, and verify with its own public key), and keep it in `dir` (mode
/// 0600, never overwriting) under the name the keyring signs with: `<domain>.<selector>.pem` for RSA,
/// `<domain>.<selector>-ed25519.pem` for Ed25519. Encrypted, weak or other kinds of keys are refused and
/// nothing is written.
pub fn import_key(dir: &Path, domain: &str, selector: &str, pem: &[u8]) -> Result<GeneratedKey> {
    check_labels(domain, selector)?;
    lib::import(dir, domain, selector, pem)
}

struct SecretFiles(Arc<dyn SecretSource>);

impl lib::KeyFiles for SecretFiles {
    fn read(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        match self.0.read(path) {
            Ok(b) => Ok(Some(b)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// The signer for `[outbound] dkim_dir`, or `None` when signing is not configured. Key files are read
/// through the `SecretSource` (mode 0600, owner, no symlink), so a loosened key file stops signing
/// loudly instead of being used.
pub fn keyring(cfg: &OutboundConfig, secrets: Arc<dyn SecretSource>) -> Option<Arc<dyn DkimSigner>> {
    let dir = cfg.dkim_dir.clone()?;
    Some(Arc::new(lib::Keyring::new(dir, cfg.dkim_selector.clone(), cfg.dkim_ed25519, Arc::new(SecretFiles(secrets)))))
}
