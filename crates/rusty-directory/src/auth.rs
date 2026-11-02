use std::sync::OnceLock;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use async_trait::async_trait;
use rand_core::OsRng;
use rusty_core::{AccountId, Error, Result};
use rusty_store::map_db_error;

use crate::dir::{Directory, Resolved};

/// Hash a password with argon2id and a random salt (PHC string). Runs on a blocking thread.
pub(crate) async fn hash_password(password: &str) -> Result<String> {
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
        .map_err(|e| Error::storage(format!("password hashing failed: {e}")))
}

fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_blocking("dummy-password-for-timing").unwrap_or_default())
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
        let res = sqlx::query("UPDATE accounts SET password_hash = ? WHERE id = ?")
            .bind(hash)
            .bind(account.0)
            .execute(self.store().pool())
            .await
            .map_err(map_db_error)?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound(format!("account {}", account.0)));
        }
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
}

impl LocalAuthenticator {
    pub fn new(directory: Directory) -> LocalAuthenticator {
        LocalAuthenticator { directory }
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
                Self::run_verify(dummy_hash().to_owned(), password).await?;
                Ok(None)
            }
        }
    }
}
