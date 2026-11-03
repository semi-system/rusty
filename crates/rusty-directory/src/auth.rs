use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use async_trait::async_trait;
use rand_core::OsRng;
use rusty_core::{AccountId, Error, Result};
use rusty_store::map_db_error;

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
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
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
        // One transaction: a new password and the revocation of every capability issued under
        // the old one become visible together.
        let mut tx = self.store().begin_write().await?;
        let res = sqlx::query("UPDATE accounts SET password_hash = ? WHERE id = ?")
            .bind(hash)
            .bind(account.0)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound("account".into()));
        }
        crate::caps::revoke_all_in(&mut tx, account, None).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(())
    }
}

#[async_trait]
pub trait Authenticator: Send + Sync {
    /// `Ok(None)` means authentication failed (for whatever reason).
    async fn verify(&self, addr: &str, password: &str) -> Result<Option<AccountId>>;
}

#[derive(Debug, Clone)]
pub struct LocalAuthenticator {
    directory: Directory,
    dummy_hash: String,
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
        let row: Option<(String, Option<String>)> = match account {
            Some(id) => sqlx::query_as("SELECT status, password_hash FROM accounts WHERE id = ?")
                .bind(id.0)
                .fetch_optional(self.directory.store().pool())
                .await
                .map_err(map_db_error)?,
            None => None,
        };
        match (account, row) {
            (Some(id), Some((status, Some(hash)))) => {
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
