//! The facades over [`SealedRepo`] (admin web design, section 14.1). They are split so that the split is a type, not a convention:
//! [`SealedWriter`] (the admin service) can store, delete and list metadata and has **no call that returns a ciphertext**;
//! [`SealedReader`] (the outbound service) can read and list metadata and cannot write. Both validate every argument before the
//! database sees it. The store never opens a blob: `rusty-seal` seals and unseals, this crate moves bytes.

use std::sync::Arc;

use rusty_core::{Error, Result};

use crate::repo::{SealedMeta, SealedRepo, SealedRow};

/// Largest sealed blob stored (the sealing overhead included): a DKIM key or a password is a few hundred bytes, 64 KiB leaves room.
pub const MAX_SEALED_BYTES: usize = 66_000;
/// Smallest blob that can be a sealed secret at all (the authentication tag alone is 16 bytes).
pub const MIN_SEALED_BYTES: usize = 16;

fn invalid(what: &'static str) -> Error {
    Error::Invalid(what.into())
}

/// `domain`, `dkim-private-key`, ...: 1 to 32 characters of `a-z 0-9 - _`.
fn label(s: &str, what: &'static str) -> Result<()> {
    if (1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
        Ok(())
    } else {
        Err(invalid(what))
    }
}

fn key_id(s: &str) -> Result<()> {
    if s.len() == 16 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        Ok(())
    } else {
        Err(invalid("invalid sealing key id"))
    }
}

/// What the admin service holds: it writes secrets and lists what exists; it cannot read one back.
#[derive(Clone)]
pub struct SealedWriter {
    repo: Arc<dyn SealedRepo>,
}

impl SealedWriter {
    pub fn with_repo(repo: Arc<dyn SealedRepo>) -> SealedWriter {
        SealedWriter { repo }
    }

    /// Store `sealed` (made by `rusty-seal` for this owner kind, id and purpose) under `key` (16 hex digits, the id of the key it was
    /// sealed to). Replaces what was there. Returns the metadata, never the ciphertext.
    pub async fn put(&self, kind: &str, id: i64, purpose: &str, key: &str, sealed: &[u8]) -> Result<SealedMeta> {
        self.put_inner(kind, id, purpose, key, sealed, None).await
    }

    #[doc(hidden)]
    pub async fn put_at(&self, kind: &str, id: i64, purpose: &str, key: &str, sealed: &[u8], now: i64) -> Result<SealedMeta> {
        crate::mailbox::check_supplied_time(now)?;
        self.put_inner(kind, id, purpose, key, sealed, Some(now)).await
    }

    async fn put_inner(&self, kind: &str, id: i64, purpose: &str, key: &str, sealed: &[u8], now: Option<i64>) -> Result<SealedMeta> {
        label(kind, "invalid owner kind")?;
        label(purpose, "invalid purpose")?;
        key_id(key)?;
        if !(MIN_SEALED_BYTES..=MAX_SEALED_BYTES).contains(&sealed.len()) {
            return Err(invalid("the sealed secret has an impossible size"));
        }
        self.repo.put(kind, id, purpose, key, sealed, now).await
    }

    /// The secrets of an owner (purpose, key id, size, times), without their ciphertext.
    pub async fn meta(&self, kind: &str, id: i64) -> Result<Vec<SealedMeta>> {
        label(kind, "invalid owner kind")?;
        self.repo.meta(kind, id).await
    }

    /// Remove a secret; whether there was one.
    pub async fn delete(&self, kind: &str, id: i64, purpose: &str) -> Result<bool> {
        label(kind, "invalid owner kind")?;
        label(purpose, "invalid purpose")?;
        self.repo.delete(kind, id, purpose).await
    }
}

/// What the outbound service holds: it reads secrets and cannot change them.
#[derive(Clone)]
pub struct SealedReader {
    repo: Arc<dyn SealedRepo>,
}

impl SealedReader {
    pub fn with_repo(repo: Arc<dyn SealedRepo>) -> SealedReader {
        SealedReader { repo }
    }

    /// The sealed secret, ciphertext included, to be opened with `rusty-seal` for the same owner kind, id and purpose.
    pub async fn get(&self, kind: &str, id: i64, purpose: &str) -> Result<Option<SealedRow>> {
        label(kind, "invalid owner kind")?;
        label(purpose, "invalid purpose")?;
        self.repo.get(kind, id, purpose).await
    }

    pub async fn meta(&self, kind: &str, id: i64) -> Result<Vec<SealedMeta>> {
        label(kind, "invalid owner kind")?;
        self.repo.meta(kind, id).await
    }
}
