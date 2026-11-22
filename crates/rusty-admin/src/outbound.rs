//! The endpoints about how a domain's mail leaves: its DKIM keys and its own relay (design sections 14.2 and 14.4). Same rules as [`crate::manage`]:
//! session and CSRF guards first, then the scope of the administrator (outside it is the same `404` as a missing thing).
//!
//! * **DKIM**: `GET` lists the public records; `POST` makes an ed25519 key whose private half is **sealed** to the outbound service and stored (the admin cannot
//!   read it back); the response carries the public record only. A key is made once; a new key is a new selector.
//! * **Relay**: `PUT` sets the domain's relay (host, port, TLS, login). The password is sealed and only ever overwritten, never returned. The rules are the ones
//!   of `rustyctl domain relay` (`rusty_core::relay_policy`: four ports, a public host, a login only over TLS). `POST .../relay/check` queues a `relay.check` job
//!   for the outbound service and the result shows in `GET`.
//! * A deployment without `[sealing]` answers `503 unavailable` where a secret would be sealed.

use std::sync::Arc;

use rusty_core::config::RelayTls;
use rusty_core::relay_policy::validate_tenant_relay;
use rusty_http::{handler, Method, Request, Response, Router};
use rusty_smtp::dkim::{generate_sealed, sealed_purpose};
use rusty_smtp::outbound::domain_route::RELAY_PASSWORD_PURPOSE;
use rusty_store::repo::RelayRow;
use rusty_store::tls_text;
use serde::{Deserialize, Serialize};

use crate::api::{fail, AdminApp, Services};
use crate::auth::Principal;

const MAX_PASSWORD: usize = 1024;
const DEFAULT_SELECTOR: &str = "rusty";

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

fn json<T: Serialize>(status: u16, v: &T) -> Response {
    Response::json(status, v).with_header("cache-control", "no-store")
}

fn services(app: &AdminApp) -> Result<&Services, Response> {
    app.services.as_ref().ok_or_else(|| err(503, "unavailable"))
}

fn body<T: for<'de> Deserialize<'de>>(req: &Request) -> Result<T, Response> {
    serde_json::from_slice(&req.body).map_err(|_| err(400, "invalid"))
}

fn no_query(req: &Request) -> Result<(), Response> {
    if req.query.is_some() {
        return Err(err(400, "invalid"));
    }
    Ok(())
}

async fn scoped(app: &AdminApp, p: &Principal, name: &str) -> Result<rusty_directory::DomainInfo, Response> {
    crate::manage::scoped_domain(app, p, name).await
}

// ------------------------------------------------------------------------------------------------ DKIM

#[derive(Serialize)]
struct DkimJson {
    selector: String,
    algorithm: String,
    record_name: String,
    record_value: String,
    /// Whether the private half is sealed in the database (the outbound service signs with it); `false` for a key made elsewhere.
    sealed: bool,
    /// When the key was made (seconds), for the age of a key that is due for rotation.
    created_at: i64,
}

fn dkim_json(selector: &str, algorithm: &str, public_key_b64: &str, domain: &str, sealed: bool, created_at: i64) -> DkimJson {
    DkimJson {
        selector: selector.to_owned(),
        algorithm: algorithm.to_owned(),
        record_name: format!("{selector}._domainkey.{domain}"),
        record_value: format!("v=DKIM1; k={algorithm}; p={public_key_b64}"),
        sealed,
        created_at,
    }
}

#[derive(Serialize)]
struct DkimList {
    items: Vec<DkimJson>,
}

async fn dkim_list(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let dom = match scoped(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let keys = match s.dns.dkim_keys(dom.id).await {
        Ok(k) => k,
        Err(e) => return fail(&e),
    };
    let sealed = match s.sealed.meta("domain", dom.id.0).await {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    let items = keys
        .iter()
        .map(|k| {
            let purpose = sealed_purpose(&k.selector);
            dkim_json(&k.selector, &k.algorithm, &k.public_key_b64, &dom.name, sealed.iter().any(|m| m.purpose == purpose), k.created_at)
        })
        .collect();
    json(200, &DkimList { items })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewDkim {
    #[serde(default)]
    selector: Option<String>,
}

async fn dkim_create(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let dom = match scoped(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let b: NewDkim = if req.body.is_empty() { NewDkim { selector: None } } else { match body(&req) {
        Ok(b) => b,
        Err(r) => return r,
    } };
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(seal) = s.sealing.as_ref() else { return err(503, "unavailable") };
    let selector = b.selector.unwrap_or_else(|| DEFAULT_SELECTOR.to_owned());
    if selector.len() > 63 {
        return err(400, "invalid");
    }
    let key = match generate_sealed(dom.id.0, &selector, seal) {
        Ok(k) => k,
        Err(e) => return fail(&e),
    };
    let existing = match s.dns.dkim_keys(dom.id).await {
        Ok(k) => k,
        Err(e) => return fail(&e),
    };
    if existing.iter().any(|k| k.selector.eq_ignore_ascii_case(&key.selector)) {
        // A published key is never replaced: a new key is a new selector.
        return err(409, "exists");
    }
    // The public row first: it is write-once, so of two concurrent requests for one selector exactly one wins (the other is `409 exists`) and only the winner
    // stores its private half. The other order could leave the loser's private key sealed under the winner's public key, and every message would be signed
    // with a key that does not verify. If sealing then fails the listing shows `sealed: false` and the administrator uses a new selector.
    match s.dns.add_dkim_key(dom.id, &key.selector, "ed25519", &key.public_key_b64).await {
        Ok((_, true)) => {}
        Ok((_, false)) => return err(409, "exists"),
        Err(e) => return fail(&e),
    }
    if let Err(e) = s.sealed.put("domain", dom.id.0, &key.purpose, &key.key_id, &key.sealed).await {
        return fail(&e);
    }
    let made = match s.dns.dkim_keys(dom.id).await {
        Ok(k) => k.into_iter().find(|k| k.selector == key.selector).map_or(0, |k| k.created_at),
        Err(e) => return fail(&e),
    };
    json(201, &dkim_json(&key.selector, "ed25519", &key.public_key_b64, &dom.name, true, made))
}

/// `DELETE /api/v1/domains/{d}/dkim/{k}`: retire a key after a rotation. The last key cannot be retired (`409 wrong_state`): a domain would then send unsigned mail. Mail signed with the old key
/// stays valid for as long as its DNS record is published, so remove the record only after the delay the receivers need (days).
async fn dkim_delete(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let dom = match scoped(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let selector = req.param("k").unwrap_or_default().to_owned();
    let keys = match s.dns.dkim_keys(dom.id).await {
        Ok(k) => k,
        Err(e) => return fail(&e),
    };
    if !keys.iter().any(|k| k.selector == selector) {
        return err(404, "not_found");
    }
    if keys.len() < 2 {
        return err(409, "wrong_state");
    }
    // The public row first: the signer lists keys from it, so signing with the selector stops at once; then the sealed private half.
    match s.dns.delete_dkim_key(dom.id, &selector).await {
        Ok(true) => {}
        Ok(false) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    }
    if let Err(e) = s.sealed.delete("domain", dom.id.0, &sealed_purpose(&selector)).await {
        return fail(&e);
    }
    json(200, &serde_json::json!({}))
}

// ------------------------------------------------------------------------------------------------ relay

#[derive(Serialize)]
struct RelayJson {
    host: String,
    port: u16,
    tls: String,
    username: Option<String>,
    spf_include: Option<String>,
    /// Whether a password is stored (it can only be overwritten, never read).
    password_set: bool,
    /// The fixed category of the last connection check, with its time (unix seconds).
    check_status: Option<String>,
    checked_at: Option<i64>,
}

#[derive(Serialize)]
struct RelayState {
    relay: Option<RelayJson>,
}

fn relay_json(r: RelayRow, password_set: bool) -> RelayJson {
    RelayJson { host: r.host, port: r.port, tls: r.tls, username: r.username, spf_include: r.spf_include, password_set, check_status: r.check_status, checked_at: r.checked_at }
}

async fn password_is_set(s: &Services, domain: i64) -> Result<bool, Response> {
    let meta = s.sealed.meta("domain", domain).await.map_err(|e| fail(&e))?;
    Ok(meta.iter().any(|m| m.purpose == RELAY_PASSWORD_PURPOSE))
}

async fn relay_get(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let dom = match scoped(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let row = match s.relays.get(dom.id).await {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let Some(row) = row else { return json(200, &RelayState { relay: None }) };
    match password_is_set(s, dom.id.0).await {
        Ok(set) => json(200, &RelayState { relay: Some(relay_json(row, set)) }),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutRelay {
    host: String,
    port: u16,
    tls: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    spf_include: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

async fn relay_put(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let dom = match scoped(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let b: PutRelay = match body(&req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let tls = match b.tls.as_str() {
        "starttls" => RelayTls::Starttls,
        "implicit" => RelayTls::Implicit,
        "none" => RelayTls::None,
        _ => return err(400, "invalid"),
    };
    let relay = match validate_tenant_relay(&b.host, b.port, tls, b.username.as_deref()) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    // A login always comes with its password in the same request: a new host never inherits the password given for another one.
    match (&b.username, &b.password) {
        (Some(_), None) | (None, Some(_)) => return err(400, "invalid"),
        (Some(_), Some(pw)) if pw.is_empty() || pw.len() > MAX_PASSWORD || pw.chars().any(char::is_control) => return err(400, "invalid"),
        _ => {}
    }
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Some(pw) = &b.password {
        let Some(seal) = s.sealing.as_ref() else { return err(503, "unavailable") };
        let sealed = match rusty_seal::Context::new("domain", dom.id.0, RELAY_PASSWORD_PURPOSE).and_then(|c| seal.seal(&c, pw.as_bytes())) {
            Ok(x) => x,
            Err(e) => return fail(&e),
        };
        if let Err(e) = s.sealed.put("domain", dom.id.0, RELAY_PASSWORD_PURPOSE, &seal.id().to_hex(), &sealed).await {
            return fail(&e);
        }
    } else if let Err(e) = s.sealed.delete("domain", dom.id.0, RELAY_PASSWORD_PURPOSE).await {
        // No login any more: a password kept from an earlier relay would be a secret nobody can see or use.
        return fail(&e);
    }
    let row = match s.relays.set(dom.id, &relay, b.spf_include.as_deref()).await {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    json(200, &RelayState { relay: Some(relay_json(row, b.password.is_some())) })
}

async fn relay_delete(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let dom = match scoped(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match s.relays.remove(dom.id).await {
        Ok(true) => {}
        Ok(false) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    }
    if let Err(e) = s.sealed.delete("domain", dom.id.0, RELAY_PASSWORD_PURPOSE).await {
        return fail(&e);
    }
    json(200, &serde_json::json!({}))
}

async fn relay_check(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let dom = match scoped(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match s.relays.get(dom.id).await {
        Ok(Some(_)) => {}
        Ok(None) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    }
    if let Err(e) = rusty_smtp::outbound::relay_job::enqueue_relay_check(&s.jobs, dom.id, &dom.name).await {
        return fail(&e);
    }
    json(200, &serde_json::json!({ "queued": true }))
}

// ------------------------------------------------------------------------------------------------ queue figures

#[derive(Serialize)]
struct QueueJson {
    queued: i64,
    running: i64,
    failed: i64,
    dead: i64,
}

/// How many outgoing messages of the domain are waiting, being sent, failed or given up (the queue's own figures; nothing about other tenants).
async fn queue_get(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let dom = match scoped(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let counts = match s.jobs.state_counts(rusty_smtp::outbound_queue::OUTBOUND_JOB_KIND, dom.id).await {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let of = |state: &str| counts.iter().find(|(s, _)| s == state).map_or(0, |(_, n)| *n);
    json(200, &QueueJson { queued: of("queued"), running: of("running"), failed: of("failed"), dead: of("dead") })
}

// ------------------------------------------------------------------------------------------------ routes

macro_rules! route {
    ($router:expr, $app:expr, $method:expr, $path:expr, |$a:ident, $r:ident| $body:expr) => {{
        let $a = $app.clone();
        $router.route($method, $path, handler(move |$r: Request| {
            let $a = $a.clone();
            async move {
                // What this request changes, told to the open streams once it succeeded (docs/design-live-updates.md).
                let topics = crate::live::topics_of(&$r);
                let resp = $body;
                $a.announce_after(&resp, topics);
                resp
            }
        }))
    }};
}

/// The routes of this part of the API.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/dkim", |a, r| dkim_list(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/dkim", |a, r| dkim_create(&a, r).await);
    let router = route!(router, app, Method::Delete, "/api/v1/domains/{d}/dkim/{k}", |a, r| dkim_delete(&a, r).await);
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/relay", |a, r| relay_get(&a, r).await);
    let router = route!(router, app, Method::Put, "/api/v1/domains/{d}/relay", |a, r| relay_put(&a, r).await);
    let router = route!(router, app, Method::Delete, "/api/v1/domains/{d}/relay", |a, r| relay_delete(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/relay/check", |a, r| relay_check(&a, r).await);
    route!(router, app, Method::Get, "/api/v1/domains/{d}/outbound", |a, r| queue_get(&a, r).await)
}

/// The TLS text stored for a mode (re-exported for the tests of the response shape).
pub fn tls_name(tls: RelayTls) -> &'static str {
    tls_text(tls)
}
