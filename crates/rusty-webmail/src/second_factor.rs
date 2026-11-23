//! The second factor of the sign-in (docs/design-2fa.md): the secrets are sealed to a key of the webmail's own (`[webmail] totp_key_file`), a code is checked against the steps around now and accepted once, and ten
//! recovery codes (hashes only) stand in for a lost device. Without the key file the factor cannot be made or checked: an account that has it on cannot sign in until the file is back, which is the safe side.

use std::path::Path;
use std::sync::Arc;

use rand_core::{OsRng, RngCore};
use rusty_core::{AccountId, Error, Result};
use rusty_directory::{hash_app_password, verify_app_password};
use rusty_seal::{Context, SealingKey, Unsealer, UnsealingKey};
use rusty_store::{TotpRepo, RECOVERY_CODES};

use crate::totp;

const PURPOSE: &str = "totp";
const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

/// Ten letters and digits in two groups of five (`abcde-fghjk`, about 49 bits each): enough for a code that is used once and then gone.
fn recovery_code() -> String {
    let limit = 256 - (256 % ALPHABET.len());
    let mut out = String::with_capacity(11);
    let mut raw = [0u8; 32];
    let mut n = 0;
    while n < 10 {
        OsRng.fill_bytes(&mut raw);
        for b in raw {
            if usize::from(b) < limit && n < 10 {
                if n == 5 {
                    out.push('-');
                }
                out.push(char::from(ALPHABET[usize::from(b) % ALPHABET.len()]));
                n += 1;
            }
        }
    }
    out
}

/// A code the way a person types it: no spaces, lower case.
fn tidy(code: &str) -> String {
    code.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase()
}

#[derive(Clone)]
pub struct SecondFactor {
    repo: Arc<dyn TotpRepo>,
    keys: Option<Arc<Keys>>,
}

struct Keys {
    sealing: SealingKey,
    unsealer: Unsealer,
}

impl SecondFactor {
    /// Without a key file the factor can be asked about but not made or checked.
    pub fn new(repo: Arc<dyn TotpRepo>, key_file: Option<&Path>, read: &dyn rusty_svc::SecretSource) -> Result<SecondFactor> {
        let keys = match key_file {
            None => None,
            Some(p) => {
                let text = String::from_utf8(read.read(p)?).map_err(|_| Error::Invalid("webmail.totp_key_file is not text".into()))?;
                let key = UnsealingKey::from_text(text.trim())?;
                let unsealer = Unsealer::new(vec![key])?;
                Some(Arc::new(Keys { sealing: unsealer.current_public(), unsealer }))
            }
        };
        Ok(SecondFactor { repo, keys })
    }

    /// Whether this service can make and check codes (the key file is there).
    pub fn available(&self) -> bool {
        self.keys.is_some()
    }

    /// Whether the account has the factor on.
    pub async fn enabled(&self, account: AccountId) -> Result<bool> {
        self.repo.enabled(account).await
    }

    /// Whether the account has begun and not finished.
    pub async fn pending(&self, account: AccountId) -> Result<bool> {
        Ok(matches!(self.repo.get(account).await?, Some(r) if !r.confirmed))
    }

    fn keys(&self) -> Result<&Keys> {
        self.keys.as_deref().ok_or_else(|| Error::Invalid("the second factor is not set up on this service".into()))
    }

    /// A new secret, sealed and kept unconfirmed: the base32 text and the `otpauth://` link to show once.
    pub async fn begin(&self, account: AccountId, address: &str) -> Result<(String, String)> {
        let keys = self.keys()?;
        let mut secret = [0u8; totp::SECRET_LEN];
        OsRng.fill_bytes(&mut secret);
        let ctx = Context::new("account", account.0, PURPOSE)?;
        let sealed = keys.sealing.seal(&ctx, &secret)?;
        self.repo.begin(account, &sealed).await?;
        Ok((totp::base32(&secret), totp::uri("Rusty mail", address, &secret)))
    }

    async fn secret(&self, account: AccountId, sealed: &[u8]) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        let keys = self.keys()?;
        let ctx = Context::new("account", account.0, PURPOSE)?;
        keys.unsealer.unseal(&ctx, sealed)
    }

    /// The person typed the first code: the factor is on and the recovery codes are made (returned in the clear, once). `None`: the code was wrong.
    pub async fn confirm(&self, account: AccountId, code: &str, now: u64) -> Result<Option<Vec<String>>> {
        let Some(row) = self.repo.get(account).await? else { return Ok(None) };
        if row.confirmed {
            return Err(Error::Conflict("already on".into()));
        }
        let secret = self.secret(account, &row.sealed).await?;
        let Some(step) = totp::verify(&secret, code, now, None) else { return Ok(None) };
        let codes: Vec<String> = (0..RECOVERY_CODES).map(|_| recovery_code()).collect();
        let mut hashes = Vec::with_capacity(codes.len());
        for c in &codes {
            hashes.push(hash_app_password(c).await?);
        }
        self.repo.confirm(account, i64::try_from(step).unwrap_or(i64::MAX), &hashes).await?;
        Ok(Some(codes))
    }

    /// A code at sign-in: a code of the app (once per step) or a recovery code (once, then gone).
    pub async fn check(&self, account: AccountId, code: &str, now: u64) -> Result<bool> {
        let Some(row) = self.repo.get(account).await? else { return Ok(false) };
        if !row.confirmed {
            return Ok(false);
        }
        let secret = self.secret(account, &row.sealed).await?;
        let typed = tidy(code);
        if typed.len() == 6 && typed.bytes().all(|b| b.is_ascii_digit()) {
            let Some(step) = totp::verify(&secret, &typed, now, row.last_step.and_then(|s| u64::try_from(s).ok())) else { return Ok(false) };
            return self.repo.advance(account, i64::try_from(step).unwrap_or(i64::MAX)).await;
        }
        for (id, hash) in self.repo.recovery(account).await? {
            if verify_app_password(&hash, &typed).await? {
                return self.repo.spend_recovery(account, id).await;
            }
        }
        Ok(false)
    }

    /// Turn it off (the caller has checked the password and a code).
    pub async fn disable(&self, account: AccountId) -> Result<()> {
        self.repo.remove(account).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recovery_code_is_two_groups_of_five_from_the_alphabet() {
        let c = recovery_code();
        assert_eq!(c.len(), 11);
        assert!(c.split('-').all(|g| g.len() == 5 && g.bytes().all(|b| ALPHABET.contains(&b))));
        assert_ne!(c, recovery_code());
        assert_eq!(tidy(" ABCDE-fghjk "), "abcde-fghjk");
    }
}
