//! HTTP Basic for the calendar and contacts clients (docs/design-caldav-carddav.md): the name and the password (the own password of the account or an app password) come with every request, so the check is
//! the one of the mail protocols (throttle per address and account, the directory, the account must be active and its plan must allow IMAP) with one addition: a password that was just verified is
//! remembered for a short time, keyed by a keyed hash of the pair (never the password), so a client that asks a dozen times in a second costs one Argon2 and not a dozen. A password changed or an app
//! password revoked is honoured when the entry expires (at most [`CACHE_SECS`]); a wrong password is never remembered.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::prelude::{Engine, BASE64_STANDARD};
use rand_core::{OsRng, RngCore};
use rusty_core::AccountId;
use rusty_dav::server::Principal;
use rusty_directory::{parse_address, AccountStatus, Authenticator, Directory};
use rusty_svc::{throttle_keys, RateLimiter, Verdict};
use sha2::{Digest, Sha256};

/// How long a verified pair is remembered.
pub const CACHE_SECS: u64 = 60;
/// How many pairs at most (a full table drops the expired and then the oldest).
const CACHE_MAX: usize = 10_000;
/// The longest `Authorization` value read: a name and a password of a few hundred bytes each.
const MAX_HEADER_BYTES: usize = 2048;

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// No credentials, or a pair that is not right: the client is asked again.
    Credentials,
    /// Too many failures: the client waits.
    Throttled(u64),
    /// The service is short of capacity or the store is not answering: try again.
    Busy,
}

pub struct DavAuth {
    authenticator: Arc<dyn Authenticator>,
    directory: Directory,
    throttle: Arc<dyn RateLimiter>,
    salt: [u8; 32],
    cache: Mutex<HashMap<[u8; 32], (Instant, AccountId, String)>>,
}

impl DavAuth {
    pub fn new(authenticator: Arc<dyn Authenticator>, directory: Directory, throttle: Arc<dyn RateLimiter>) -> DavAuth {
        let mut salt = [0u8; 32];
        OsRng.fill_bytes(&mut salt);
        DavAuth { authenticator, directory, throttle, salt, cache: Mutex::new(HashMap::new()) }
    }

    /// The name and the password of a `Authorization: Basic` value.
    pub fn credentials(header: Option<&str>) -> Option<(String, String)> {
        let h = header?;
        if h.len() > MAX_HEADER_BYTES {
            return None;
        }
        let (scheme, rest) = h.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("basic") {
            return None;
        }
        let raw = BASE64_STANDARD.decode(rest.trim()).ok()?;
        let text = String::from_utf8(raw).ok()?;
        let (user, password) = text.split_once(':')?;
        (!user.is_empty() && !password.is_empty() && !user.chars().any(char::is_control)).then(|| (user.to_owned(), password.to_owned()))
    }

    fn key(&self, user: &str, password: &str) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(self.salt);
        h.update((user.len() as u64).to_be_bytes());
        h.update(user.as_bytes());
        h.update(password.as_bytes());
        h.finalize().into()
    }

    fn cached(&self, key: &[u8; 32]) -> Option<(AccountId, String)> {
        let cache = self.cache.lock().ok()?;
        let (at, account, address) = cache.get(key)?;
        (at.elapsed() < Duration::from_secs(CACHE_SECS)).then(|| (*account, address.clone()))
    }

    fn remember(&self, key: [u8; 32], account: AccountId, address: String) {
        let Ok(mut cache) = self.cache.lock() else { return };
        if cache.len() >= CACHE_MAX {
            let ttl = Duration::from_secs(CACHE_SECS);
            cache.retain(|_, (at, _, _)| at.elapsed() < ttl);
            if cache.len() >= CACHE_MAX {
                let oldest = cache.iter().min_by_key(|(_, (at, _, _))| *at).map(|(k, _)| *k);
                if let Some(k) = oldest {
                    cache.remove(&k);
                }
            }
        }
        cache.insert(key, (Instant::now(), account, address));
    }

    /// Check the credentials of a request from `peer`.
    pub async fn check(&self, peer: IpAddr, header: Option<&str>) -> Result<Principal, Refused> {
        let Some((user, password)) = Self::credentials(header) else { return Err(Refused::Credentials) };
        let address = parse_address(&user).map_or_else(|_| user.trim().to_ascii_lowercase(), |(l, d)| format!("{l}@{d}"));
        let key = self.key(&address, &password);
        if let Some((account, address)) = self.cached(&key) {
            return Ok(Principal { account, address });
        }
        let keys = throttle_keys(&self.directory, &address).await;
        for k in &keys {
            if let Verdict::Blocked { retry_after_secs } = self.throttle.check(peer, k).await {
                return Err(Refused::Throttled(retry_after_secs));
            }
        }
        let Some(_permit) = self.throttle.try_begin_attempt() else { return Err(Refused::Busy) };
        let fail = || async {
            for k in &keys {
                self.throttle.record_failure(peer, k).await;
            }
            Err(Refused::Credentials)
        };
        let account = match self.authenticator.verify(&address, &password).await {
            Ok(Some(a)) => a,
            Ok(None) => return fail().await,
            Err(_) => return Err(Refused::Busy),
        };
        match self.directory.account_status(account).await {
            Ok(Some(AccountStatus::Active)) => {}
            Ok(_) => return fail().await,
            Err(_) => return Err(Refused::Busy),
        }
        // The calendars are part of what a person does with the mailbox: the plans that allow IMAP allow them.
        match self.directory.protocol_allowed(account, "imap").await {
            Ok(true) => {}
            Ok(false) => return fail().await,
            Err(_) => return Err(Refused::Busy),
        }
        for k in &keys {
            self.throttle.record_success(peer, k).await;
        }
        self.remember(key, account, address.clone());
        Ok(Principal { account, address })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_read_from_a_basic_header_only() {
        let b = |s: &str| format!("Basic {}", BASE64_STANDARD.encode(s));
        assert_eq!(DavAuth::credentials(Some(&b("ada@one.test:pw:with:colons"))), Some(("ada@one.test".into(), "pw:with:colons".into())));
        assert_eq!(DavAuth::credentials(Some(&format!("basic {}", BASE64_STANDARD.encode("a:b")))), Some(("a".into(), "b".into())));
        for bad in [None, Some("Bearer abc"), Some("Basic"), Some("Basic !!!"), Some(&b("nocolon")[..]), Some(&b(":pw")[..]), Some(&b("a:")[..]), Some(&b("a\u{1}b:pw")[..])] {
            assert_eq!(DavAuth::credentials(bad), None, "{bad:?}");
        }
        assert_eq!(DavAuth::credentials(Some(&format!("Basic {}", "A".repeat(3000)))), None);
    }
}
