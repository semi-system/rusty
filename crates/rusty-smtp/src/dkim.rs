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
use std::time::{Duration, Instant};

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


/// What the outbound worker signs with. `Ok(vec![])` when the domain has no key (the message goes unsigned); an error when a key
/// exists and cannot be used (the worker then defers the mail: never silently unsigned). Asynchronous because a sealed key is read
/// from the database; a key-file keyring is one too through [`message_signer`].
#[async_trait::async_trait]
pub trait MessageSigner: Send + Sync {
    async fn sign(&self, domain: &str, message: &[u8]) -> Result<Vec<u8>>;
}

struct FileSigner(Arc<dyn DkimSigner>);

#[async_trait::async_trait]
impl MessageSigner for FileSigner {
    async fn sign(&self, domain: &str, message: &[u8]) -> Result<Vec<u8>> {
        self.0.sign(domain, message)
    }
}

/// A key-file keyring as a [`MessageSigner`].
pub fn message_signer(files: Arc<dyn DkimSigner>) -> Arc<dyn MessageSigner> {
    Arc::new(FileSigner(files))
}

/// The `purpose` under which the sealed private key of a DKIM selector is stored (`sealed_secrets`): `dkim-` and 24 hex digits of
/// the SHA-256 of the lower-cased selector. A selector may hold dots and capitals, which a purpose may not, and DNS does not tell
/// `Rusty` from `rusty`.
pub fn sealed_purpose(selector: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("dkim-{}", hex::encode(Sha256::digest(selector.to_ascii_lowercase().as_bytes()).get(..12).unwrap_or_default()))
}


/// A DKIM key made to be stored sealed: the private key exists only inside this call and in the sealed blob.
pub struct SealedKey {
    /// The selector the record is published under (`<selector>-ed25519`).
    pub selector: String,
    /// Base64 of the public key (`p=`).
    pub public_key_b64: String,
    /// The id of the sealing key the blob was sealed to.
    pub key_id: String,
    /// `sealed_secrets.purpose`: [`sealed_purpose`] of the selector.
    pub purpose: String,
    /// The sealed private key (PEM), bound to `("domain", domain_id, purpose)`.
    pub sealed: Vec<u8>,
}

impl SealedKey {
    /// The TXT value to publish.
    pub fn txt_value(&self) -> String {
        format!("v=DKIM1; k=ed25519; p={}", self.public_key_b64)
    }
}

/// Make an Ed25519 key for the domain with id `domain_id` and seal its private half to `seal_to` (admin web design, section 14.2). Nothing is
/// written to a file. The caller stores the blob (`SealedWriter::put`) and the public half (`DnsStore::add_dkim_key`). The published selector is
/// `<selector>-ed25519`, like the key files of `rustyctl dkim generate`.
pub fn generate_sealed(domain_id: i64, selector: &str, seal_to: &rusty_seal::SealingKey) -> Result<SealedKey> {
    if !safe_label(selector) {
        return Err(Error::Invalid("selector is not valid for a DKIM key".into()));
    }
    let published = format!("{selector}-ed25519");
    let (pem, public_key_b64) = lib::generate_ed25519_pem()?;
    let purpose = sealed_purpose(&published);
    let sealed = seal_to.seal(&rusty_seal::Context::new("domain", domain_id, &purpose)?, pem.as_bytes())?;
    Ok(SealedKey { selector: published, public_key_b64, key_id: seal_to.id().to_hex(), purpose, sealed })
}

const SEALED_TTL: Duration = Duration::from_secs(60);

/// The signers loaded for a domain and when.
type SealedEntry = (Instant, Vec<Arc<lib::Signer>>);

/// Signs with the sealed private keys of a domain (admin web design, section 14): every key in `domain_dkim_keys` whose private half
/// was sealed to this service. The keys are read from the database, opened with the unsealing key(s) of this service, loaded, and
/// kept for a short time (a removed or replaced secret is noticed within the ttl). A key whose private half is not here (made
/// elsewhere, public row only) is not signed with; a key that is here and cannot be opened is an **error**, never a silent skip.
pub struct SealedKeyring {
    directory: rusty_directory::Directory,
    dns: rusty_store::DnsStore,
    reader: rusty_store::SealedReader,
    unsealer: rusty_seal::Unsealer,
    ttl: Duration,
    cache: std::sync::Mutex<std::collections::HashMap<String, SealedEntry>>,
}

impl SealedKeyring {
    pub fn new(directory: rusty_directory::Directory, dns: rusty_store::DnsStore, reader: rusty_store::SealedReader, unsealer: rusty_seal::Unsealer) -> SealedKeyring {
        SealedKeyring { directory, dns, reader, unsealer, ttl: SEALED_TTL, cache: Default::default() }
    }

    /// How long loaded keys are kept (zero: every signature reads the database).
    pub fn with_ttl(mut self, ttl: Duration) -> SealedKeyring {
        self.ttl = ttl;
        self
    }

    async fn signers(&self, domain: &str) -> Result<Vec<Arc<lib::Signer>>> {
        let now = Instant::now();
        if self.ttl > Duration::ZERO {
            if let Ok(c) = self.cache.lock() {
                if let Some((at, v)) = c.get(domain) {
                    if now.duration_since(*at) < self.ttl {
                        return Ok(v.clone());
                    }
                }
            }
        }
        let mut out = Vec::new();
        if let Some(dom) = self.directory.find_domain(domain).await? {
            for key in self.dns.dkim_keys(dom.id).await?.iter().take(rusty_core::dns::MAX_DKIM_KEYS) {
                let purpose = sealed_purpose(&key.selector);
                let Some(row) = self.reader.get("domain", dom.id.0, &purpose).await? else { continue };
                let context = rusty_seal::Context::new("domain", dom.id.0, &purpose)?;
                let pem = self.unsealer.unseal(&context, &row.sealed)?;
                out.push(Arc::new(lib::load_signer(&pem, domain, &key.selector, key.algorithm == "ed25519")?));
            }
        }
        if self.ttl > Duration::ZERO {
            if let Ok(mut c) = self.cache.lock() {
                c.insert(domain.to_owned(), (now, out.clone()));
            }
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl MessageSigner for SealedKeyring {
    async fn sign(&self, domain: &str, message: &[u8]) -> Result<Vec<u8>> {
        let mut out = String::new();
        for s in self.signers(domain).await? {
            out.push_str(&s.sign(message)?);
        }
        Ok(out.into_bytes())
    }
}

struct Chain {
    sealed: Option<Arc<SealedKeyring>>,
    files: Option<Arc<dyn DkimSigner>>,
}

#[async_trait::async_trait]
impl MessageSigner for Chain {
    async fn sign(&self, domain: &str, message: &[u8]) -> Result<Vec<u8>> {
        // Sealed keys win: a domain that has any signs with them only, so one selector never signs twice. A domain without a sealed
        // key keeps signing with its key files, so an installation that does not use sealed keys changes nothing.
        if let Some(sealed) = &self.sealed {
            let sig = sealed.sign(domain, message).await?;
            if !sig.is_empty() {
                return Ok(sig);
            }
        }
        match &self.files {
            Some(f) => f.sign(domain, message),
            None => Ok(Vec::new()),
        }
    }
}

/// The signer the worker uses: the sealed keyring first, the key-file keyring for domains that have no sealed key.
pub fn chain(sealed: Option<Arc<SealedKeyring>>, files: Option<Arc<dyn DkimSigner>>) -> Arc<dyn MessageSigner> {
    Arc::new(Chain { sealed, files })
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
