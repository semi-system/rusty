//! DKIM signing as a Rusty-owned seam: `DkimSigner` (sign a message for a domain), key generation for
//! `rustyctl dkim generate`, and the public record text. The crypto and the signature format are
//! behind the DKIM library adapter.
//!
//! Policy (user decision): RSA-2048 by default for the widest receiver compatibility, an optional
//! ed25519 second signature; relaxed/relaxed canonicalisation; the signed header list includes
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
    Rsa2048,
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
            DkimAlgo::Rsa2048 => format!("v=DKIM1; k=rsa; p={}", self.public_key_b64),
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

/// Create a key for `domain` in `dir` (mode 0600, never overwriting an existing key).
pub fn generate_key(dir: &Path, domain: &str, selector: &str, algo: DkimAlgo) -> Result<GeneratedKey> {
    if !safe_label(domain) || !safe_label(selector) {
        return Err(Error::Invalid("domain or selector is not valid for a DKIM key file".into()));
    }
    lib::generate(dir, domain, selector, algo)
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
