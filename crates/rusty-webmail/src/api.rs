//! The HTTP API of the webmail, session part (docs/design-webmail.md section 4), over `rusty-http`.
//!
//! * Every response carries `Cache-Control: no-store`; errors are `{"error":"<fixed code>"}` and never carry backend text, a path or what the user sent.
//! * `POST /api/v1/session` signs in with the mail address and password (Origin and JSON body checked; there is no CSRF token yet) and answers with **one fixed error** for an unknown address, a wrong
//!   password and a disabled account; `DELETE` signs out (Origin, JSON content type and CSRF token all checked); `GET /api/v1/me` says who is signed in and gives the CSRF token.
//! * [`WebmailApp::require`] is what every later endpoint calls: the session from the cookie, the account's status from the database **now** (a disabled account is signed out at its next request),
//!   and for a state-changing request the three CSRF guards.
//! * The sign-in uses the directory's own authenticator (Argon2, constant work for unknown accounts) and the same [`RateLimiter`] the IMAP and submission logins use, per source address and per account.

use std::net::IpAddr;
use std::sync::Arc;

use rusty_core::{AccountId, Result};
use rusty_directory::{parse_address, AccountStatus, Authenticator, Directory};
use rusty_http::cookie::{clear_cookie, session_cookie};
use rusty_http::guard::{guard_mutation, is_json, origin_ok, pick_origin};
use rusty_http::{handler, Method, Request, Response, Router};
use rusty_svc::{throttle_keys, RateLimiter, Verdict};
use serde::{Deserialize, Serialize};

use crate::session::{Session, SessionStore};

/// The session cookie (`__Host-`: `Secure`, `Path=/`, no `Domain`).
pub const COOKIE: &str = "__Host-rusty_webmail";
const MAX_ADDRESS: usize = 320;
const MAX_PASSWORD: usize = 1024;

/// Why a sign-in did not happen. The texts the HTTP layer shows are fixed per variant and never say which part was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInError {
    /// Too many failures from this address or for this account: try again after the delay.
    Throttled { retry_after_secs: u64 },
    /// Wrong credentials, or an account that is not active. Always the same.
    Invalid,
    /// Too many sign-ins are being checked at once (password hashing is expensive).
    Busy,
    /// The database or the session store could not answer.
    Unavailable,
}

/// The webmail's authentication: sign in, find the account behind a session, sign out.
#[derive(Clone)]
pub struct WebmailAuth {
    authenticator: Arc<dyn Authenticator>,
    directory: Directory,
    sessions: SessionStore,
    throttle: Arc<dyn RateLimiter>,
}

impl WebmailAuth {
    pub fn new(authenticator: Arc<dyn Authenticator>, directory: Directory, sessions: SessionStore, throttle: Arc<dyn RateLimiter>) -> WebmailAuth {
        WebmailAuth { authenticator, directory, sessions, throttle }
    }

    pub fn sessions(&self) -> &SessionStore {
        &self.sessions
    }

    /// Check the credentials (throttled, and only for an active account): the account, and the throttle keys to record the success under.
    async fn check(&self, peer: IpAddr, address: &str, password: &str) -> std::result::Result<(AccountId, Vec<String>), SignInError> {
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
        match self.directory.account_status(account).await {
            Ok(Some(AccountStatus::Active)) => {}
            // The password is right but the account may not use mail now: the same answer, and it counts as a failure.
            Ok(_) => {
                fail(&keys).await;
                return Err(SignInError::Invalid);
            }
            Err(_) => return Err(SignInError::Unavailable),
        }
        Ok((account, keys))
    }

    /// Check the credentials and open a session. Returns the session id (for the cookie) and the session.
    pub async fn sign_in(&self, peer: IpAddr, address: &str, password: &str) -> std::result::Result<(String, Session), SignInError> {
        let (account, keys) = self.check(peer, address, password).await?;
        let label = parse_address(address).map_or_else(|_| address.trim().to_ascii_lowercase(), |(l, d)| format!("{l}@{d}"));
        let opened = self.sessions.create(account, &label).await.map_err(|_| SignInError::Unavailable)?;
        for k in &keys {
            self.throttle.record_success(peer, k).await;
        }
        Ok(opened)
    }

    /// The signed-in person proves it is them again (before something a stolen session must not be able to do): the same checks and the same throttle as a sign-in, without a new session.
    pub async fn confirm(&self, peer: IpAddr, address: &str, password: &str) -> std::result::Result<AccountId, SignInError> {
        let (account, keys) = self.check(peer, address, password).await?;
        for k in &keys {
            self.throttle.record_success(peer, k).await;
        }
        Ok(account)
    }

    /// The signed-in account behind a session id **now**: the session must be live and the account active. `None` is "not signed in" (a session whose account has gone is also ended). `Err`: a store could
    /// not answer, treated as not signed in.
    pub async fn session(&self, session_id: &str) -> Result<Option<Session>> {
        let Some(session) = self.sessions.get(session_id).await? else { return Ok(None) };
        if matches!(self.directory.account_status(session.account_id()).await?, Some(AccountStatus::Active)) {
            Ok(Some(session))
        } else {
            self.sessions.destroy(session_id).await?;
            Ok(None)
        }
    }

    pub async fn sign_out(&self, session_id: &str) -> Result<()> {
        self.sessions.destroy(session_id).await
    }

    /// End every session of an account (its password changed, it was disabled).
    pub async fn revoke_account(&self, account: AccountId) -> Result<()> {
        self.sessions.revoke_account(account).await
    }
}

/// What the endpoints share.
pub struct WebmailApp {
    pub auth: WebmailAuth,
    /// Opens the mailbox of an account: the read path asks it for the mail of the account behind the session and for nothing else.
    pub hub: rusty_svc::MailboxHub,
    /// The platform blob store, where a composed message is put before it is queued.
    pub blobs: Arc<dyn rusty_store::BlobStore>,
    pub outbox: Arc<dyn rusty_store::OutboxRepo>,
    pub app_passwords: Arc<dyn rusty_store::AppPasswordRepo>,
    pub settings: Arc<dyn rusty_store::WebmailSettingsRepo>,
    /// The most bytes of a message the person may send.
    pub max_message_bytes: usize,
    /// `https://host[:port]` of the webmail host name, as the browser sends it in `Origin`.
    pub public_origin: String,
    /// Other origins it is reached by (`[webmail] extra_origins`).
    pub extra_origins: Vec<String>,
}

impl WebmailApp {
    /// The session and the account of a request; for a state-changing request the Origin, JSON and CSRF token checks too.
    pub async fn require(&self, req: &Request, mutating: bool) -> std::result::Result<Session, Response> {
        let Some(id) = req.cookie(COOKIE) else { return Err(error(401, "unauthorized")) };
        let session = match self.auth.session(id).await {
            Ok(Some(s)) => s,
            Ok(None) => return Err(error(401, "unauthorized")),
            Err(_) => return Err(error(503, "busy")),
        };
        if mutating {
            guard_mutation(req, pick_origin(req, &self.public_origin, &self.extra_origins), &session.csrf).map_err(no_store)?;
        }
        Ok(session)
    }
}

fn no_store(r: Response) -> Response {
    r.with_header("cache-control", "no-store")
}

pub fn error(status: u16, code: &'static str) -> Response {
    no_store(Response::error(status, code))
}

fn json<T: Serialize>(status: u16, v: &T) -> Response {
    no_store(Response::json(status, v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignInBody {
    address: String,
    password: String,
}

#[derive(Serialize)]
struct Me<'a> {
    address: &'a str,
    csrf: &'a str,
}

fn me(s: &Session) -> Me<'_> {
    Me { address: &s.address, csrf: &s.csrf }
}

fn pre_auth_guard(app: &WebmailApp, req: &Request) -> std::result::Result<(), Response> {
    if !origin_ok(req, pick_origin(req, &app.public_origin, &app.extra_origins)) {
        return Err(error(403, "forbidden"));
    }
    if !is_json(req) {
        return Err(error(415, "unsupported"));
    }
    Ok(())
}

async fn sign_in(app: &WebmailApp, req: Request) -> Response {
    if let Err(r) = pre_auth_guard(app, &req) {
        return r;
    }
    let Ok(b) = serde_json::from_slice::<SignInBody>(&req.body) else { return error(400, "invalid") };
    if b.address.len() > MAX_ADDRESS || b.password.len() > MAX_PASSWORD {
        return error(400, "invalid");
    }
    match app.auth.sign_in(req.peer.addr.ip(), &b.address, &b.password).await {
        Ok((id, s)) => match session_cookie(COOKIE, &id, app.auth.sessions().config().absolute) {
            Some(c) => json(200, &me(&s)).with_header("set-cookie", c),
            None => error(500, "internal"),
        },
        Err(SignInError::Throttled { retry_after_secs }) => error(429, "throttled").with_header("retry-after", retry_after_secs.max(1).to_string()),
        Err(SignInError::Invalid) => error(401, "unauthorized"),
        Err(SignInError::Busy | SignInError::Unavailable) => error(503, "busy"),
    }
}

async fn whoami(app: &WebmailApp, req: Request) -> Response {
    match app.require(&req, false).await {
        Ok(s) => json(200, &me(&s)),
        Err(r) => r,
    }
}

async fn sign_out(app: &WebmailApp, req: Request) -> Response {
    if let Err(r) = app.require(&req, true).await {
        return r;
    }
    if let Some(id) = req.cookie(COOKIE) {
        if app.auth.sign_out(id).await.is_err() {
            return error(503, "busy");
        }
    }
    match clear_cookie(COOKIE) {
        Some(c) => json(200, &serde_json::json!({})).with_header("set-cookie", c),
        None => error(500, "internal"),
    }
}

/// The routes of this part of the API.
pub fn routes(app: Arc<WebmailApp>, router: Router) -> Router {
    macro_rules! route {
        ($router:expr, $method:expr, $path:expr, $f:ident) => {{
            let a = app.clone();
            $router.route($method, $path, handler(move |r: Request| {
                let a = a.clone();
                async move { $f(&a, r).await }
            }))
        }};
    }
    let router = route!(router, Method::Post, "/api/v1/session", sign_in);
    let router = route!(router, Method::Delete, "/api/v1/session", sign_out);
    route!(router, Method::Get, "/api/v1/me", whoami)
}
