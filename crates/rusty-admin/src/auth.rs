//! Sign-in and the per-request authorization of the admin web (design section 5).
//!
//! * **Sign-in** uses the directory's own authenticator (Argon2, constant work for unknown accounts), the same [`RateLimiter`] the IMAP and
//!   submission logins use (keys of the resolved account and of the canonical address, per source address), and answers with **one fixed error**
//!   for an unknown account, a wrong password, a disabled account and an account that is not an administrator.
//! * **Every request** is authorized from the database, not from what the session remembered: [`AdminAuth::principal`] re-reads the grant and
//!   the account status, so removing a grant or disabling an account takes effect at the next request, whatever the session says.

use std::net::IpAddr;
use std::sync::Arc;

use rusty_core::{AccountId, DomainId, Result};
use rusty_directory::{Authenticator, Directory};
use rusty_store::AdminStore;
use rusty_svc::{throttle_keys, RateLimiter, Verdict};

use crate::session::{Session, SessionStore};

/// What an administrator may administer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every domain; may create domains.
    Platform,
    /// One domain only.
    Domain(DomainId),
}

impl Scope {
    /// May this administrator act on `domain`? A platform administrator may on every domain, a domain administrator on its own only.
    pub fn allows(self, domain: DomainId) -> bool {
        match self {
            Scope::Platform => true,
            Scope::Domain(d) => d == domain,
        }
    }

    pub fn is_platform(self) -> bool {
        self == Scope::Platform
    }
}

/// A signed-in administrator, as the database says it is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Principal {
    pub account: AccountId,
    pub scope: Scope,
    /// A helpdesk administrator: a domain administrator that may only look at accounts, reset passwords and lock or unlock (see `Principal::may`).
    pub helpdesk: bool,
}

impl Principal {
    /// May this administrator make this request? Everyone but a helpdesk administrator may (the handlers then check the scope); a helpdesk administrator only the few that its role is for.
    pub fn may(&self, method: &str, path: &str) -> bool {
        if !self.helpdesk {
            return true;
        }
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        matches!(
            (method, segs.as_slice()),
            ("GET", ["api", "v1", "session"] | ["api", "v1", "domains"] | ["api", "v1", "domains", _, "accounts" | "overview"]) | ("POST", ["api", "v1", "accounts", _, "password" | "lock" | "enable"])
        )
    }
}

/// Why a sign-in did not happen. The texts the HTTP layer shows are fixed per variant and never say which part was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInError {
    /// Too many failures from this address or for this account: try again after the delay.
    Throttled { retry_after_secs: u64 },
    /// Wrong credentials, a disabled account, or an account that is not an administrator. Always the same.
    Invalid,
    /// Too many sign-ins are being checked at once (password hashing is expensive).
    Busy,
    /// The database or the session store could not answer.
    Unavailable,
}

#[derive(Clone)]
pub struct AdminAuth {
    authenticator: Arc<dyn Authenticator>,
    directory: Directory,
    admins: AdminStore,
    sessions: SessionStore,
    throttle: Arc<dyn RateLimiter>,
}

impl AdminAuth {
    pub fn new(
        authenticator: Arc<dyn Authenticator>,
        directory: Directory,
        admins: AdminStore,
        sessions: SessionStore,
        throttle: Arc<dyn RateLimiter>,
    ) -> AdminAuth {
        AdminAuth { authenticator, directory, admins, sessions, throttle }
    }

    pub fn sessions(&self) -> &SessionStore {
        &self.sessions
    }

    /// Check the credentials and open a session. Returns the session id (for the cookie) and the session.
    pub async fn sign_in(&self, peer: IpAddr, address: &str, password: &str) -> std::result::Result<(String, Session), SignInError> {
        let keys = throttle_keys(&self.directory, address).await;
        for k in &keys {
            if let Verdict::Blocked { retry_after_secs } = self.throttle.check(peer, k).await {
                return Err(SignInError::Throttled { retry_after_secs });
            }
        }
        let Some(_permit) = self.throttle.try_begin_attempt() else { return Err(SignInError::Busy) };
        let fail = |keys: &[String]| {
            let throttle = self.throttle.clone();
            let keys = keys.to_vec();
            async move {
                for k in &keys {
                    throttle.record_failure(peer, k).await;
                }
            }
        };
        let account = match self.authenticator.verify(address, password).await {
            Ok(Some(a)) => a,
            Ok(None) => {
                fail(&keys).await;
                return Err(SignInError::Invalid);
            }
            Err(_) => return Err(SignInError::Unavailable),
        };
        let grant = match self.admins.get(account).await {
            Ok(Some(g)) => g,
            // The password is right but the account administers nothing: the same answer, and it counts as a failure.
            Ok(None) => {
                fail(&keys).await;
                return Err(SignInError::Invalid);
            }
            Err(_) => return Err(SignInError::Unavailable),
        };
        let label = rusty_directory::parse_address(address).map_or_else(|_| address.trim().to_ascii_lowercase(), |(l, d)| format!("{l}@{d}"));
        let opened = self.sessions.create(account, grant.domain, &label).await.map_err(|_| SignInError::Unavailable)?;
        for k in &keys {
            self.throttle.record_success(peer, k).await;
        }
        Ok(opened)
    }

    /// The administrator behind a session id **now**: the session must be live, the account active, and the grant still there (its scope is the
    /// database's, not the session's). `None` is "not signed in" (including a session whose grant or account has gone, which is also ended).
    /// `Err` means a store could not answer: treat as not signed in.
    pub async fn principal(&self, session_id: &str) -> Result<Option<(Session, Principal)>> {
        self.principal_of(session_id, true).await
    }

    /// [`AdminAuth::principal`] for a watcher: the same checks, but the session's idle time is not renewed.
    pub async fn principal_peek(&self, session_id: &str) -> Result<Option<(Session, Principal)>> {
        self.principal_of(session_id, false).await
    }

    async fn principal_of(&self, session_id: &str, renew: bool) -> Result<Option<(Session, Principal)>> {
        let found = if renew { self.sessions.get(session_id).await? } else { self.sessions.peek(session_id).await? };
        let Some(session) = found else { return Ok(None) };
        let account = session.account_id();
        let active = matches!(self.directory.account_status(account).await?, Some(rusty_directory::AccountStatus::Active));
        let grant = if active { self.admins.get(account).await? } else { None };
        let Some(grant) = grant else {
            self.sessions.destroy(session_id).await?;
            return Ok(None);
        };
        let scope = grant.domain.map_or(Scope::Platform, Scope::Domain);
        Ok(Some((session, Principal { account, scope, helpdesk: grant.helpdesk })))
    }

    pub async fn sign_out(&self, session_id: &str) -> Result<()> {
        self.sessions.destroy(session_id).await
    }

    /// End every session of an account (its password changed, it was disabled, its grant was removed).
    pub async fn revoke_account(&self, account: AccountId) -> Result<()> {
        self.sessions.revoke_account(account).await
    }
}
