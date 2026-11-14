//! Sealed secrets (admin web design, section 14.1).
//!
//! A secret that the web creates or receives (a DKIM private key, a tenant's relay password) is **encrypted to the outbound
//! service's public key** (HPKE, RFC 9180), bound to its owner and purpose, and stored. The admin can write one and overwrite
//! it; it cannot read it back, because only the outbound service holds the private key ([`UnsealingKey`]).
//!
//! * **Context binding.** The owner kind, the owner id and the purpose are part of the encryption context, so a stored blob
//!   cannot be moved to another domain or used for another purpose: opening it for a different [`Context`] fails.
//! * **One error.** Anything wrong with a blob (damaged, truncated, for another key, another context, another version)
//!   gives the same fixed error: no oracle, and never any of the plaintext.
//! * **Rotation.** A blob carries the id of the key it was sealed to. An [`Unsealer`] holds up to four keys, picks by id, and
//!   its first key is the one new secrets are sealed to.
//! * **Format** (version 1): `0x01 | key id (8 bytes) | encapsulated key (32) | ciphertext and tag`. The overhead is [`OVERHEAD`]
//!   bytes; the plaintext is at most [`MAX_PLAINTEXT`]. A golden test pins the format: stored secrets must stay openable.
//!
//! The HPKE library is named only inside `adapter` (the wrapped-libraries gate). Random numbers come from the operating
//! system; if it cannot give any, the process panics (as the library documents), which is the safe outcome for a key generator.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

mod adapter;

use std::fmt;

use rusty_core::{Error, Result};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use adapter::KEY_LEN;

/// Largest secret that can be sealed (a DKIM key or a password is a few hundred bytes).
pub const MAX_PLAINTEXT: usize = 64 * 1024;
/// What sealing adds to the plaintext: version, key id, encapsulated key and the authentication tag.
pub const OVERHEAD: usize = 1 + KEY_ID_LEN + KEY_LEN + 16;
/// Most keys an [`Unsealer`] holds (the current one, and the ones a rotation still needs).
pub const MAX_KEYS: usize = 4;

const VERSION: u8 = 1;
const KEY_ID_LEN: usize = 8;
const SEAL_PREFIX: &str = "rusty-seal-v1:";
const UNSEAL_PREFIX: &str = "rusty-unseal-v1:";
/// Domain separation of the HPKE `info`: a blob made for another purpose of the same library cannot be confused with ours.
const INFO_TAG: &[u8] = b"rusty-seal-v1\0";

fn cannot_open() -> Error {
    Error::Invalid("the sealed secret cannot be opened".into())
}

fn invalid(what: &'static str) -> Error {
    Error::Invalid(what.into())
}

/// What a secret belongs to and what it is for. Bound into the encryption: a blob opens only for the context it was sealed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context<'a> {
    kind: &'a str,
    id: i64,
    purpose: &'a str,
}

fn label_ok(s: &str) -> bool {
    (1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

impl<'a> Context<'a> {
    /// `kind` (`domain`, `tenant`, ...) and `purpose` (`dkim-private-key`, `relay-password`, ...) are 1 to 32 characters of
    /// `a-z 0-9 - _`; `id` is the owner's row id.
    pub fn new(kind: &'a str, id: i64, purpose: &'a str) -> Result<Context<'a>> {
        if !label_ok(kind) || !label_ok(purpose) {
            return Err(invalid("invalid sealing context"));
        }
        Ok(Context { kind, id, purpose })
    }

    /// The bytes bound into the encryption: fixed tag, then each field with its length, so no two contexts share an encoding.
    fn info(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(INFO_TAG.len() + 4 + self.kind.len() + 8 + self.purpose.len());
        v.extend_from_slice(INFO_TAG);
        v.extend_from_slice(&(self.kind.len() as u16).to_be_bytes());
        v.extend_from_slice(self.kind.as_bytes());
        v.extend_from_slice(&self.id.to_be_bytes());
        v.extend_from_slice(&(self.purpose.len() as u16).to_be_bytes());
        v.extend_from_slice(self.purpose.as_bytes());
        v
    }
}

/// The short id of a public key (the first 8 bytes of its SHA-256): written into every blob and shown to the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyId([u8; KEY_ID_LEN]);

impl KeyId {
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

fn id_of(public: &[u8; KEY_LEN]) -> KeyId {
    let digest = Sha256::digest(public);
    let mut id = [0u8; KEY_ID_LEN];
    id.copy_from_slice(digest.get(..KEY_ID_LEN).unwrap_or_default());
    KeyId(id)
}

/// The public key secrets are sealed to. Not a secret: the admin holds it.
#[derive(Clone, PartialEq, Eq)]
pub struct SealingKey {
    public: [u8; KEY_LEN],
}

impl fmt::Debug for SealingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SealingKey({})", self.id().to_hex())
    }
}

impl SealingKey {
    /// From the 32 bytes of the public key. A key that is not a proper public key, or of small order, is refused.
    pub fn from_bytes(public: [u8; KEY_LEN]) -> Result<SealingKey> {
        adapter::check_public(&public).map_err(|_| invalid("invalid sealing key"))?;
        Ok(SealingKey { public })
    }

    pub fn to_bytes(&self) -> [u8; KEY_LEN] {
        self.public
    }

    pub fn id(&self) -> KeyId {
        id_of(&self.public)
    }

    /// `rusty-seal-v1:<64 hex digits>`.
    pub fn to_text(&self) -> String {
        format!("{SEAL_PREFIX}{}", hex::encode(self.public))
    }

    pub fn from_text(text: &str) -> Result<SealingKey> {
        let hex = text.trim().strip_prefix(SEAL_PREFIX).ok_or_else(|| invalid("invalid sealing key"))?;
        SealingKey::from_bytes(decode_key(hex).ok_or_else(|| invalid("invalid sealing key"))?)
    }

    /// Seal `plaintext` (at most [`MAX_PLAINTEXT`] bytes) for `context`. The result is what is stored.
    pub fn seal(&self, context: &Context<'_>, plaintext: &[u8]) -> Result<Vec<u8>> {
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(invalid("the secret is too large to be sealed"));
        }
        let id = self.id();
        let (enc, ct) = adapter::seal(&self.public, &context.info(), &id.0, plaintext).map_err(|_| invalid("cannot seal the secret"))?;
        let mut out = Vec::with_capacity(OVERHEAD + plaintext.len());
        out.push(VERSION);
        out.extend_from_slice(&id.0);
        out.extend_from_slice(&enc);
        out.extend_from_slice(&ct);
        Ok(out)
    }
}

fn decode_key(hex_text: &str) -> Option<[u8; KEY_LEN]> {
    let bytes = hex::decode(hex_text).ok()?;
    <[u8; KEY_LEN]>::try_from(bytes).ok()
}

/// The private key only the outbound service holds. Never copied, never printed, wiped when dropped.
pub struct UnsealingKey {
    secret: Zeroizing<[u8; KEY_LEN]>,
    public: [u8; KEY_LEN],
}

impl fmt::Debug for UnsealingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UnsealingKey({}, <redacted>)", id_of(&self.public).to_hex())
    }
}

/// A new key pair: the private half for the outbound service, the public half for everything that seals.
pub fn generate() -> (UnsealingKey, SealingKey) {
    let (secret, public) = adapter::generate();
    (UnsealingKey { secret, public }, SealingKey { public })
}

impl UnsealingKey {
    pub fn public(&self) -> SealingKey {
        SealingKey { public: self.public }
    }

    /// `rusty-unseal-v1:<64 hex digits>`: what the operator's key file holds (mode 0600). Wiped when dropped.
    pub fn to_text(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("{UNSEAL_PREFIX}{}", hex::encode(*self.secret)))
    }

    pub fn from_text(text: &str) -> Result<UnsealingKey> {
        let bad = || invalid("invalid unsealing key");
        let hex_text = text.trim().strip_prefix(UNSEAL_PREFIX).ok_or_else(bad)?;
        let secret = Zeroizing::new(decode_key(hex_text).ok_or_else(bad)?);
        let public = adapter::public_of(&secret).map_err(|_| bad())?;
        Ok(UnsealingKey { secret, public })
    }

    fn id(&self) -> KeyId {
        id_of(&self.public)
    }
}

/// The keys the outbound service opens secrets with: the current one first (new secrets are sealed to its public half),
/// then those a rotation still needs.
pub struct Unsealer {
    keys: Vec<UnsealingKey>,
}

impl Unsealer {
    /// One to [`MAX_KEYS`] distinct keys.
    pub fn new(keys: Vec<UnsealingKey>) -> Result<Unsealer> {
        if keys.is_empty() || keys.len() > MAX_KEYS {
            return Err(invalid("an unsealer needs one to four keys"));
        }
        for (n, k) in keys.iter().enumerate() {
            if keys.iter().take(n).any(|o| o.id() == k.id()) {
                return Err(invalid("two unsealing keys are the same key"));
            }
        }
        Ok(Unsealer { keys })
    }

    /// The public half of the current key: what the admin should be given to seal with.
    pub fn current_public(&self) -> SealingKey {
        self.keys.first().map_or_else(|| SealingKey { public: [0u8; KEY_LEN] }, UnsealingKey::public)
    }

    /// Open a blob made by [`SealingKey::seal`] for the same `context`. Every failure is one fixed error.
    pub fn unseal(&self, context: &Context<'_>, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let (version, rest) = sealed.split_first().ok_or_else(cannot_open)?;
        if *version != VERSION || sealed.len() < OVERHEAD {
            return Err(cannot_open());
        }
        let (id, rest) = rest.split_at_checked(KEY_ID_LEN).ok_or_else(cannot_open)?;
        let (enc, ciphertext) = rest.split_at_checked(KEY_LEN).ok_or_else(cannot_open)?;
        let enc: [u8; KEY_LEN] = enc.try_into().map_err(|_| cannot_open())?;
        let key = self.keys.iter().find(|k| k.id().0 == id).ok_or_else(cannot_open)?;
        adapter::open(&key.secret, &enc, &context.info(), id, ciphertext).map_err(|_| cannot_open())
    }
}
