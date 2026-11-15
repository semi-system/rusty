//! The HTTP API of the admin web, session and setup part (design sections 4, 5 and 14.3), over `rusty-http`.
//!
//! * Every response carries `Cache-Control: no-store`; errors are `{"error":"<fixed code>"}` and never carry backend text, a path or what the user sent.
//! * `POST /api/v1/session` signs in (Origin and JSON body checked; there is no CSRF token yet), `GET` says who is signed in and gives the CSRF token,
//!   `DELETE` signs out (Origin, JSON content type and CSRF token all checked).
//! * `GET` and `POST /api/v1/setup` exist only while no platform administrator exists, a database fact read on every request: afterwards they are `404`.
//! * [`AdminApp::require`] is what every later endpoint calls: the session from the cookie, the administrator from the database, and for a
//!   state-changing request the three CSRF guards.

use std::sync::Arc;
use std::time::Duration;

use rusty_core::Error;
use rusty_directory::{parse_address, Directory};
use rusty_http::cookie::{clear_cookie, session_cookie};
use rusty_http::guard::{guard_mutation, is_json, origin_ok};
use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::AdminStore;
use serde::{Deserialize, Serialize};

use crate::auth::{AdminAuth, Principal, Scope, SignInError};
use crate::session::{Session, SessionConfig};
use crate::setup::{SetupGate, Verdict};

/// The session cookie (`__Host-`: `Secure`, `Path=/`, no `Domain`).
pub const COOKIE: &str = "__Host-rusty_admin";
/// The shortest password the first administrator may choose.
pub const MIN_ADMIN_PASSWORD: usize = 12;
const MAX_ADDRESS: usize = 320;
const MAX_PASSWORD: usize = 1024;
const MAX_TOKEN: usize = 128;

/// The operator's facts the DNS page needs (from `[admin]` and the shared `hostname`).
#[derive(Debug, Clone)]
pub struct MailSettings {
    pub host: String,
    pub public_ips: Vec<std::net::IpAddr>,
    pub spf_include: Option<String>,
}

/// The stores the outbound endpoints (DKIM, relay) need beyond the directory; absent in a deployment that only manages domains and accounts.
pub struct Services {
    pub dns: rusty_store::DnsStore,
    pub relays: rusty_store::RelayStore,
    pub sealed: rusty_store::SealedWriter,
    pub jobs: rusty_store::JobQueue,
    /// `[sealing] public_key`: what secrets are sealed to. `None`: nothing can be sealed, the endpoints that need it answer `503 unavailable`.
    pub sealing: Option<rusty_seal::SealingKey>,
    /// `None` (no host name configured): the DNS page answers `503 unavailable`.
    pub mail: Option<MailSettings>,
}

pub struct AdminApp {
    pub auth: AdminAuth,
    pub directory: Directory,
    pub admins: AdminStore,
    /// `Some` when the service started with no administrator (the token was printed); `None` otherwise.
    pub setup: Option<SetupGate>,
    /// `https://host[:port]` of the admin host name, as the browser sends it in `Origin`.
    pub public_origin: String,
    pub session_cfg: SessionConfig,
    pub services: Option<Services>,
    /// Serialises the setup, so two requests cannot both make a first administrator.
    setup_lock: tokio::sync::Mutex<()>,
}

impl AdminApp {
    pub fn new(auth: AdminAuth, directory: Directory, admins: AdminStore, setup: Option<SetupGate>, public_origin: &str, session_cfg: SessionConfig) -> AdminApp {
        AdminApp { auth, directory, admins, setup, public_origin: public_origin.to_owned(), session_cfg, services: None, setup_lock: tokio::sync::Mutex::new(()) }
    }

    /// Give the app the stores of the outbound endpoints.
    pub fn with_services(mut self, services: Services) -> AdminApp {
        self.services = Some(services);
        self
    }

    /// The signed-in administrator of a request. `Err` is the response to send: `401 unauthorized` (no or ended session), `503 busy` (a store
    /// cannot answer: never treated as signed in), `403`/`415` from the CSRF guards when `mutating`.
    pub async fn require(&self, req: &Request, mutating: bool) -> Result<(Session, Principal), Response> {
        let Some(id) = req.cookie(COOKIE) else { return Err(error(401, "unauthorized")) };
        let found = match self.auth.principal(id).await {
            Ok(f) => f,
            Err(_) => return Err(error(503, "busy")),
        };
        let Some((session, principal)) = found else { return Err(error(401, "unauthorized")) };
        if mutating {
            guard_mutation(req, &self.public_origin, &session.csrf).map_err(no_store)?;
        }
        Ok((session, principal))
    }
}

fn no_store(r: Response) -> Response {
    r.with_header("cache-control", "no-store")
}

fn error(status: u16, code: &'static str) -> Response {
    no_store(Response::error(status, code))
}

fn json<T: Serialize>(status: u16, v: &T) -> Response {
    no_store(Response::json(status, v))
}

/// Backend errors as the fixed codes of the API; the text never leaves the process.
pub fn fail(e: &Error) -> Response {
    match e {
        Error::Invalid(_) => error(400, "invalid"),
        Error::NotFound(_) => error(404, "not_found"),
        Error::Conflict(_) => error(409, "exists"),
        Error::Quota(_) => error(429, "throttled"),
        _ => error(503, "busy"),
    }
}

/// A JSON body of exactly the fields of `T`; anything else is `400 invalid`.
fn body<T: for<'de> Deserialize<'de>>(req: &Request) -> Result<T, Response> {
    serde_json::from_slice(&req.body).map_err(|_| error(400, "invalid"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignInBody {
    address: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupBody {
    token: String,
    address: String,
    password: String,
}

#[derive(Serialize)]
struct Me<'a> {
    address: &'a str,
    /// `platform` or `domain`.
    scope: &'static str,
    /// The domain id of a domain administrator.
    domain: Option<i64>,
    csrf: &'a str,
}

fn me(session: &Session, scope: Scope) -> Me<'_> {
    let (scope_text, domain) = match scope {
        Scope::Platform => ("platform", None),
        Scope::Domain(d) => ("domain", Some(d.0)),
    };
    Me { address: &session.address, scope: scope_text, domain, csrf: &session.csrf }
}

/// The session cookie for a new session: it lives as long as the absolute limit (the server ends it earlier when it is idle).
fn with_cookie(r: Response, id: &str, cfg: &SessionConfig) -> Response {
    match session_cookie(COOKIE, id, cfg.absolute) {
        Some(c) => r.with_header("set-cookie", c),
        None => error(500, "internal"),
    }
}

fn pre_auth_guard(app: &AdminApp, req: &Request) -> Result<(), Response> {
    if !origin_ok(req, &app.public_origin) {
        return Err(error(403, "forbidden"));
    }
    if !is_json(req) {
        return Err(error(415, "unsupported"));
    }
    Ok(())
}

async fn sign_in(app: &AdminApp, req: Request) -> Response {
    if let Err(r) = pre_auth_guard(app, &req) {
        return r;
    }
    let b: SignInBody = match body(&req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if b.address.len() > MAX_ADDRESS || b.password.len() > MAX_PASSWORD {
        return error(400, "invalid");
    }
    match app.auth.sign_in(req.peer.addr.ip(), &b.address, &b.password).await {
        Ok((id, s)) => {
            let scope = s.domain_id().map_or(Scope::Platform, Scope::Domain);
            with_cookie(json(200, &me(&s, scope)), &id, &app.session_cfg)
        }
        Err(SignInError::Throttled { retry_after_secs }) => error(429, "throttled").with_header("retry-after", retry_after_secs.max(1).to_string()),
        Err(SignInError::Invalid) => error(401, "unauthorized"),
        Err(SignInError::Busy | SignInError::Unavailable) => error(503, "busy"),
    }
}

async fn whoami(app: &AdminApp, req: Request) -> Response {
    match app.require(&req, false).await {
        Ok((s, p)) => json(200, &me(&s, p.scope)),
        Err(r) => r,
    }
}

async fn sign_out(app: &AdminApp, req: Request) -> Response {
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

/// `404` unless setup is open: the page exists only while no platform administrator exists (a database fact, read now) and a token is outstanding.
async fn setup_open(app: &AdminApp) -> Result<&SetupGate, Response> {
    let Some(gate) = app.setup.as_ref().filter(|g| g.is_live()) else { return Err(error(404, "not_found")) };
    match app.admins.has_platform_admin().await {
        Ok(false) => Ok(gate),
        Ok(true) => Err(error(404, "not_found")),
        Err(_) => Err(error(503, "busy")),
    }
}

async fn setup_state(app: &AdminApp, _req: Request) -> Response {
    match setup_open(app).await {
        Ok(_) => json(200, &serde_json::json!({ "required": true })),
        Err(r) => r,
    }
}

async fn setup(app: &AdminApp, req: Request) -> Response {
    if let Err(r) = pre_auth_guard(app, &req) {
        return r;
    }
    let gate = match setup_open(app).await {
        Ok(g) => g,
        Err(r) => return r,
    };
    let b: SetupBody = match body(&req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if b.token.len() > MAX_TOKEN || b.address.len() > MAX_ADDRESS || b.password.len() > MAX_PASSWORD {
        return error(400, "invalid");
    }
    let peer = req.peer.addr.ip();
    match gate.check(peer, &b.token) {
        Verdict::Accepted => {}
        Verdict::Wrong => return error(401, "unauthorized"),
        Verdict::Locked { retry_after_secs } => return error(429, "throttled").with_header("retry-after", retry_after_secs.max(1).to_string()),
    }
    // The token was right. Check what was typed before anything is made.
    let Ok((local, domain)) = parse_address(&b.address) else { return error(400, "invalid") };
    if b.password.chars().count() < MIN_ADMIN_PASSWORD {
        return error(400, "invalid");
    }
    let _one_at_a_time = app.setup_lock.lock().await;
    // Re-check under the lock: a second request that was waiting must find the administrator the first one made.
    match app.admins.has_platform_admin().await {
        Ok(false) => {}
        Ok(true) => return error(404, "not_found"),
        Err(_) => return error(503, "busy"),
    }
    let domain_id = match app.directory.find_domain(&domain).await {
        Ok(Some(d)) => d.id,
        Ok(None) => match app.directory.create_domain(&domain).await {
            Ok(id) => id,
            Err(e) => return fail(&e),
        },
        Err(e) => return fail(&e),
    };
    let account = match app.directory.create_account(&local, domain_id, Some(&b.password), None).await {
        Ok(a) => a,
        Err(e) => return fail(&e),
    };
    if let Err(e) = app.admins.grant(account, None).await {
        return fail(&e);
    }
    gate.consume();
    // Signed in at once, through the normal path (and its throttle).
    match app.auth.sign_in(peer, &b.address, &b.password).await {
        Ok((id, s)) => with_cookie(json(201, &me(&s, Scope::Platform)), &id, &app.session_cfg),
        Err(_) => json(201, &serde_json::json!({ "created": true })),
    }
}

/// The routes of this part of the API (later parts add theirs to the same router).
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    let a = app.clone();
    let router = router.route(Method::Post, "/api/v1/session", handler(move |req| {
        let a = a.clone();
        async move { sign_in(&a, req).await }
    }));
    let a = app.clone();
    let router = router.route(Method::Get, "/api/v1/session", handler(move |req| {
        let a = a.clone();
        async move { whoami(&a, req).await }
    }));
    let a = app.clone();
    let router = router.route(Method::Delete, "/api/v1/session", handler(move |req| {
        let a = a.clone();
        async move { sign_out(&a, req).await }
    }));
    let a = app.clone();
    let router = router.route(Method::Get, "/api/v1/setup", handler(move |req| {
        let a = a.clone();
        async move { setup_state(&a, req).await }
    }));
    let a = app;
    router.route(Method::Post, "/api/v1/setup", handler(move |req| {
        let a = a.clone();
        async move { setup(&a, req).await }
    }))
}

/// Idle sessions end by themselves; this is the cookie lifetime used when the server wants a shorter one.
pub fn cookie_lifetime(cfg: &SessionConfig) -> Duration {
    cfg.absolute
}
