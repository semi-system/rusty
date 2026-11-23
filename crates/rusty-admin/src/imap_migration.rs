//! The move of a mailbox from another IMAP server (docs/design-imap-migration.md). A platform administrator gives the other server (host, port, how it is protected), the login and the password; the
//! password is sealed at once (the admin service can seal it and cannot read it back) and the IMAP service, where the mailboxes are, pulls the mail with its dates and flags and reports its progress in
//! the database, which this module serves. Every request that changes something is in the audit log (the password is hidden there, as every secret is).

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::{Migration, NewMigration, MIGRATION_SECRET_OWNER, MIGRATION_SECRET_PURPOSE};
use serde::{Deserialize, Serialize};

use crate::api::{fail, AdminApp, Services};
use crate::manage::{platform_only, scoped_account};

const MAX_PASSWORD: usize = 1024;

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

#[derive(Serialize)]
struct MigrationJson {
    id: i64,
    host: String,
    port: i32,
    security: String,
    user: String,
    requested_by: String,
    /// `preparing`, `queued`, `running`, `done`, `failed` or `cancelled`.
    state: String,
    /// The administrator asked it to stop and it has not yet.
    stopping: bool,
    /// The folder of the other server being copied now.
    folder: Option<String>,
    folders_total: i32,
    folders_done: i32,
    messages: i64,
    skipped: i64,
    bytes: i64,
    /// A fixed word when it failed (`connect`, `tls`, `auth`, `protocol`, `timeout`, `quota`, `too_large`, `failed`).
    error: Option<String>,
    created_at: i64,
    finished_at: Option<i64>,
}

#[derive(Serialize)]
struct MigrationList {
    items: Vec<MigrationJson>,
}

fn migration_json(m: Migration) -> MigrationJson {
    MigrationJson {
        id: m.id,
        host: m.host,
        port: m.port,
        security: m.security,
        user: m.remote_user,
        requested_by: m.requested_by,
        stopping: m.cancel_requested && m.state == "running",
        state: m.state,
        folder: m.folder,
        folders_total: m.folders_total,
        folders_done: m.folders_done,
        messages: m.messages,
        skipped: m.skipped,
        bytes: m.bytes,
        error: m.error,
        created_at: m.created_at,
        finished_at: m.finished_at,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    host: String,
    port: i32,
    /// `tls`, `starttls` or `plain`.
    security: String,
    user: String,
    password: String,
}

/// A host name or an address: letters, digits, `.`, `-`, `:` (an IPv6 literal) and `_`, one to 253 characters.
fn host_ok(h: &str) -> bool {
    (1..=253).contains(&h.len()) && h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'))
}

fn text_ok(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control)
}

macro_rules! begin {
    ($app:expr, $req:expr, $mutating:expr) => {{
        let (session, p) = match $app.require(&$req, $mutating).await {
            Ok(x) => x,
            Err(r) => return r,
        };
        if let Err(r) = platform_only(&p) {
            return r;
        }
        let Some(services) = $app.services.as_ref() else { return err(503, "unavailable") };
        (session, p, services)
    }};
}

/// `POST /api/v1/accounts/{a}/migrations`: ask for the move.
async fn start(app: &AdminApp, req: Request) -> Response {
    let (session, p, s) = begin!(app, req, true);
    let (acc, _) = match scoped_account(app, &p, req.param("a").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<Start>(&req.body) else { return err(400, "invalid") };
    let host = b.host.trim().to_ascii_lowercase();
    if !host_ok(&host) || !(1..=65535).contains(&b.port) || !["tls", "starttls", "plain"].contains(&b.security.as_str()) || !text_ok(&b.user, 320) || b.password.is_empty() || b.password.chars().count() > MAX_PASSWORD || b.password.chars().any(char::is_control) {
        return err(400, "invalid");
    }
    let Some(seal) = s.sealing.as_ref() else { return err(503, "unavailable") };
    // Only one at a time for an account: a second would copy the same mail again.
    match s.migrations.list_for_account(acc.id.0, 5).await {
        Ok(rows) if rows.iter().any(|m| matches!(m.state.as_str(), "preparing" | "queued" | "running")) => return err(409, "wrong_state"),
        Ok(_) => {}
        Err(e) => return fail(&e),
    }
    let id = match s.migrations.create(&NewMigration { account: acc.id.0, requested_by: session.address.clone(), host, port: b.port, security: b.security.clone(), remote_user: b.user.trim().to_owned() }).await {
        Ok(id) => id,
        Err(e) => return fail(&e),
    };
    // The password is sealed under this migration; the worker takes the migration only after that.
    let sealed = match rusty_seal::Context::new(MIGRATION_SECRET_OWNER, id, MIGRATION_SECRET_PURPOSE).and_then(|c| seal.seal(&c, b.password.as_bytes())) {
        Ok(x) => x,
        Err(e) => {
            let _ = s.migrations.request_cancel(id).await;
            return fail(&e);
        }
    };
    if let Err(e) = s.sealed.put(MIGRATION_SECRET_OWNER, id, MIGRATION_SECRET_PURPOSE, &seal.id().to_hex(), &sealed).await {
        let _ = s.migrations.request_cancel(id).await;
        return fail(&e);
    }
    match s.migrations.queue(id).await {
        Ok(true) => got(s, id, 201).await,
        Ok(false) => err(409, "wrong_state"),
        Err(e) => fail(&e),
    }
}

async fn got(s: &Services, id: i64, status: u16) -> Response {
    match s.migrations.get(id).await {
        Ok(Some(m)) => Response::json(status, &migration_json(m)).with_header("cache-control", "no-store"),
        Ok(None) => err(404, "not_found"),
        Err(e) => fail(&e),
    }
}

/// `GET /api/v1/accounts/{a}/migrations`: the newest ten of the account, newest first.
async fn list(app: &AdminApp, req: Request) -> Response {
    let (_, p, s) = begin!(app, req, false);
    let (acc, _) = match scoped_account(app, &p, req.param("a").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match s.migrations.list_for_account(acc.id.0, 10).await {
        Ok(rows) => Response::json(200, &MigrationList { items: rows.into_iter().map(migration_json).collect() }).with_header("cache-control", "no-store"),
        Err(e) => fail(&e),
    }
}

/// `DELETE /api/v1/accounts/{a}/migrations/{m}`: stop it. A waiting one is cancelled at once, a running one stops after the message it is on.
async fn cancel(app: &AdminApp, req: Request) -> Response {
    let (_, p, s) = begin!(app, req, true);
    let (acc, _) = match scoped_account(app, &p, req.param("a").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(id) = req.param("m").and_then(|m| m.parse::<i64>().ok()).filter(|n| *n > 0) else { return err(404, "not_found") };
    match s.migrations.get(id).await {
        Ok(Some(m)) if m.account == acc.id.0 => {}
        Ok(_) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    }
    match s.migrations.request_cancel(id).await {
        Ok(true) => got(s, id, 200).await,
        Ok(false) => err(409, "wrong_state"),
        Err(e) => fail(&e),
    }
}

pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    macro_rules! route {
        ($router:expr, $method:expr, $path:expr, $f:ident) => {{
            let a = app.clone();
            $router.route($method, $path, handler(move |r: Request| {
                let a = a.clone();
                async move { $f(&a, r).await }
            }))
        }};
    }
    let router = route!(router, Method::Post, "/api/v1/accounts/{a}/migrations", start);
    let router = route!(router, Method::Get, "/api/v1/accounts/{a}/migrations", list);
    route!(router, Method::Delete, "/api/v1/accounts/{a}/migrations/{m}", cancel)
}
