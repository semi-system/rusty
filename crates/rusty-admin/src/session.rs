//! Sessions of the admin web (design section 5).
//!
//! A session id is 256 random bits from the operating system's generator, shown to the browser as 64 lower-case hex digits (the cookie value).
//! The server keeps **only `SHA-256(id)`** as the key, so a read of the store does not give a usable id. The value is a small JSON record
//! (account, grant scope at sign-in, CSRF token, creation and last-seen time) in the [`Kv`] store: Valkey when configured, the in-process store
//! otherwise.
//!
//! * **Idle timeout** is the key's time to live, renewed at every use; **absolute timeout** is checked against the creation time.
//! * **Revocation of an account** (its password changed, it was disabled, its grant removed) writes a marker `admin:revoked:<account>`; a session
//!   created before the marker is not honoured. No index of an account's sessions is needed.
//! * **Fails closed**: when the store cannot answer, [`SessionStore::get`] returns an error, never "signed in".
//! * A malformed id is "no session" without touching the store.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rand_core::{OsRng, RngCore};
use rusty_core::{AccountId, DomainId, Error, Result};
use rusty_valkey::Kv;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Seconds since the Unix epoch, injectable so tests need no sleeping.
pub trait Clock: Send + Sync {
    fn unix_secs(&self) -> u64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemNow;

impl Clock for SystemNow {
    fn unix_secs(&self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
    }
}

/// A clock that moves only when told to (tests).
#[derive(Debug)]
pub struct ManualNow(AtomicU64);

impl ManualNow {
    pub fn new(start: u64) -> ManualNow {
        ManualNow(AtomicU64::new(start))
    }
    pub fn advance(&self, d: Duration) {
        self.0.fetch_add(d.as_secs(), Ordering::SeqCst);
    }
}

impl Clock for ManualNow {
    fn unix_secs(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionConfig {
    /// A session unused for this long ends (30 minutes).
    pub idle: Duration,
    /// A session ends this long after sign-in, used or not (8 hours).
    pub absolute: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig { idle: Duration::from_secs(30 * 60), absolute: Duration::from_secs(8 * 60 * 60) }
    }
}

/// What a session holds. The scope is the grant at sign-in and is **not trusted** for decisions: `AdminAuth` re-reads the grant on every request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub account: i64,
    /// The address it signed in with (normalised), for display.
    pub address: String,
    /// `None`: platform administrator at sign-in.
    pub domain: Option<i64>,
    /// The value `X-CSRF-Token` must carry on a state-changing request.
    pub csrf: String,
    pub created: u64,
    pub last_seen: u64,
}

impl Session {
    pub fn account_id(&self) -> AccountId {
        AccountId(self.account)
    }
    pub fn domain_id(&self) -> Option<DomainId> {
        self.domain.map(DomainId)
    }
}

/// Length of a session id in characters (256 bits as hex).
pub const SESSION_ID_LEN: usize = 64;

#[derive(Clone)]
pub struct SessionStore {
    kv: Arc<dyn Kv>,
    cfg: SessionConfig,
    clock: Arc<dyn Clock>,
}

fn random_hex() -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

fn id_ok(id: &str) -> bool {
    id.len() == SESSION_ID_LEN && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn session_key(id: &str) -> String {
    format!("admin:sess:{}", hex::encode(Sha256::digest(id.as_bytes())))
}

fn revoked_key(account: i64) -> String {
    format!("admin:revoked:{account}")
}

impl SessionStore {
    pub fn new(kv: Arc<dyn Kv>, cfg: SessionConfig) -> SessionStore {
        SessionStore { kv, cfg, clock: Arc::new(SystemNow) }
    }

    /// With a given clock (tests).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> SessionStore {
        self.clock = clock;
        self
    }

    /// Open a session for `account`. The id is new at every sign-in (a session id is never reused or kept across a sign-in). Returns the id (for
    /// the cookie) and the record.
    pub async fn create(&self, account: AccountId, domain: Option<DomainId>, address: &str) -> Result<(String, Session)> {
        let now = self.clock.unix_secs();
        let id = random_hex();
        let session = Session { account: account.0, address: address.to_owned(), domain: domain.map(|d| d.0), csrf: random_hex(), created: now, last_seen: now };
        let bytes = serde_json::to_vec(&session).map_err(|_| Error::storage("session encoding failed"))?;
        self.kv.set(&session_key(&id), &bytes, self.cfg.idle).await?;
        Ok((id, session))
    }

    /// The live session of `id`, with its idle time renewed; `None` when there is none (unknown, expired, revoked, malformed).
    /// `Err` when the store cannot answer: the caller must treat that as "not signed in" and not as a reason to let anyone in.
    pub async fn get(&self, id: &str) -> Result<Option<Session>> {
        self.lookup(id, true).await
    }

    /// The same checks as [`SessionStore::get`], **without** renewing the idle time: for what only watches a session (an open event stream must not keep it alive).
    pub async fn peek(&self, id: &str) -> Result<Option<Session>> {
        self.lookup(id, false).await
    }

    async fn lookup(&self, id: &str, renew: bool) -> Result<Option<Session>> {
        if !id_ok(id) {
            return Ok(None);
        }
        let key = session_key(id);
        let Some(bytes) = self.kv.get(&key).await? else { return Ok(None) };
        let Ok(mut session) = serde_json::from_slice::<Session>(&bytes) else {
            // Not something we wrote: drop it.
            self.kv.delete(&key).await?;
            return Ok(None);
        };
        let now = self.clock.unix_secs();
        if now.saturating_sub(session.created) >= self.cfg.absolute.as_secs() {
            self.kv.delete(&key).await?;
            return Ok(None);
        }
        if let Some(marker) = self.kv.get(&revoked_key(session.account)).await? {
            let at = String::from_utf8_lossy(&marker).parse::<u64>().unwrap_or(u64::MAX);
            if session.created <= at {
                self.kv.delete(&key).await?;
                return Ok(None);
            }
        }
        session.last_seen = now;
        // Renew the idle time. A session that vanished between the read and now is gone (touch creates nothing).
        if renew && !self.kv.touch(&key, self.cfg.idle).await? {
            return Ok(None);
        }
        Ok(Some(session))
    }

    /// End one session (sign out); a missing one is fine.
    pub async fn destroy(&self, id: &str) -> Result<()> {
        if id_ok(id) {
            self.kv.delete(&session_key(id)).await?;
        }
        Ok(())
    }

    /// End **every** session of an account made up to now: its password changed, it was disabled, its grant was removed. Sessions made after this
    /// call are not affected (the person signs in again).
    pub async fn revoke_account(&self, account: AccountId) -> Result<()> {
        let now = self.clock.unix_secs();
        self.kv.set(&revoked_key(account.0), now.to_string().as_bytes(), self.cfg.absolute).await
    }
}
