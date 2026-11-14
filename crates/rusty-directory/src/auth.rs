use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use async_trait::async_trait;
use rusty_core::{AccountId, Error, Result};
use rusty_store::repo::AuthRow;

use crate::dir::{Directory, Resolved};

/// Longest accepted password, in bytes.
pub(crate) const MAX_PASSWORD_BYTES: usize = 1024;

/// Hash a password with argon2id and a random salt (PHC string). Runs on a blocking thread.
pub(crate) async fn hash_password(password: &str) -> Result<String> {
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(Error::Invalid("password too long".into()));
    }
    if password.is_empty() {
        return Err(Error::Invalid("empty password".into()));
    }
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || hash_blocking(&password))
        .await
        .map_err(|e| Error::storage_with("hash task failed", e))?
}

fn hash_blocking(password: &str) -> Result<String> {
    // argon2 0.6 draws a fresh random salt itself (getrandom), no RNG is passed in.
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|_| Error::storage("password hashing failed"))
}

fn verify_blocking(hash: &str, password: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(h) => Argon2::default()
            .verify_password(password.as_bytes(), &h)
            .is_ok(),
        Err(_) => false,
    }
}

impl Directory {
    pub async fn set_password(&self, account: AccountId, password: &str) -> Result<()> {
        let hash = hash_password(password).await?;
        // One transaction in the repository: a new password and the revocation of every
        // capability issued under the old one become visible together.
        self.repo().set_password_hash(account, &hash).await
    }
}

#[async_trait]
pub trait Authenticator: Send + Sync {
    /// `Ok(None)` means authentication failed (for whatever reason).
    async fn verify(&self, addr: &str, password: &str) -> Result<Option<AccountId>>;
}

#[derive(Clone)]
pub struct LocalAuthenticator {
    directory: Directory,
    dummy_hash: String,
}

impl std::fmt::Debug for LocalAuthenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalAuthenticator").finish_non_exhaustive()
    }
}

impl LocalAuthenticator {
    /// Computes the dummy hash (same Argon2 params as real hashes) used to equalize timing.
    pub async fn new(directory: Directory) -> Result<LocalAuthenticator> {
        let dummy_hash = hash_password("dummy-password-for-timing").await?;
        Ok(LocalAuthenticator {
            directory,
            dummy_hash,
        })
    }

    async fn run_verify(hash: String, password: &str) -> Result<bool> {
        let password = password.to_owned();
        tokio::task::spawn_blocking(move || verify_blocking(&hash, &password))
            .await
            .map_err(|e| Error::storage_with("verify task failed", e))
    }
}

#[async_trait]
impl Authenticator for LocalAuthenticator {
    async fn verify(&self, addr: &str, password: &str) -> Result<Option<AccountId>> {
        if password.len() > MAX_PASSWORD_BYTES {
            return Ok(None);
        }
        let account = match self.directory.resolve(addr).await {
            Ok(Resolved::Account(id)) => Some(id),
            Ok(_) | Err(Error::NotFound(_)) | Err(Error::Invalid(_)) => None,
            Err(e) => return Err(e),
        };
        let row = match account {
            Some(id) => self.directory.repo().account_auth(id).await?,
            None => None,
        };
        match (account, row) {
            (Some(id), Some(AuthRow { status, password_hash: Some(hash) })) => {
                let ok = Self::run_verify(hash, password).await?;
                Ok((ok && status == "active").then_some(id))
            }
            _ => {
                // Unknown account or no local hash: burn comparable time.
                Self::run_verify(self.dummy_hash.clone(), password).await?;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Produced by argon2 0.5.3 with `Argon2::default()`. Hashes already stored in the database
    /// must keep verifying after the crate is upgraded; if this fails, users are locked out.
    const GOLDEN_FROM_0_5: &str =
        "$argon2id$v=19$m=19456,t=2,p=1$TlqsjKgyrt1jew18nQqtxw$739NvvgRWiK1bUJ8ytixD04G+WmAjJz0UlggXSfaVI8";

    #[test]
    fn a_hash_made_by_argon2_0_5_still_verifies() {
        assert!(verify_blocking(GOLDEN_FROM_0_5, "correct horse battery staple"));
        assert!(!verify_blocking(GOLDEN_FROM_0_5, "correct horse battery stapl"));
        assert!(!verify_blocking(GOLDEN_FROM_0_5, ""));
    }

    #[test]
    fn new_hashes_keep_the_same_parameters_and_round_trip() {
        let h = hash_blocking("another password").unwrap();
        assert!(
            h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "default parameters changed: {h}"
        );
        assert!(verify_blocking(&h, "another password"));
        assert!(!verify_blocking(&h, "Another password"));
        let other = hash_blocking("another password").unwrap();
        assert_ne!(h, other, "salts must be random");
    }

    #[test]
    fn garbage_hashes_never_verify() {
        assert!(!verify_blocking("not a phc string", "x"));
        assert!(!verify_blocking("", "x"));
    }
}
