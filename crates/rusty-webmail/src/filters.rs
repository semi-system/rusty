//! The filters of the person (docs/design-sieve.md, step 4): Sieve scripts kept in the database and checked by the parser before they are kept. `GET /api/v1/sieve` lists them, `GET /api/v1/sieve/{name}` reads one,
//! `PUT /api/v1/sieve/{name}` (`{"body": ".."}`) keeps one after the parser accepted it, `POST /api/v1/sieve/{name}/activate` and `POST /api/v1/sieve/deactivate` choose the one that runs on the incoming mail, and
//! `DELETE /api/v1/sieve/{name}` removes one that is not active. A script the parser refuses is answered with the line and the words of the parser (its own fixed texts, never anything of the script).

use std::sync::Arc;

use rusty_core::Error;
use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::{MAX_SCRIPT_BYTES, MAX_SCRIPTS};
use serde::{Deserialize, Serialize};

use crate::api::{error, WebmailApp};

fn json<T: Serialize>(status: u16, v: &T) -> Response {
    Response::json(status, v).with_header("cache-control", "no-store")
}

/// A name of a script: what a person types in a list (letters, digits, space and a few marks), 1 to 64 characters.
fn valid_name(n: &str) -> bool {
    (1..=64).contains(&n.chars().count()) && n.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.')) && n.trim() == n
}

fn name_of(req: &Request) -> Option<String> {
    req.param("name").map(str::to_owned).filter(|n| valid_name(n))
}

#[derive(Serialize)]
struct ScriptJson {
    name: String,
    active: bool,
    size: usize,
}

#[derive(Serialize)]
struct ListJson {
    scripts: Vec<ScriptJson>,
    max_scripts: i64,
    max_bytes: usize,
}

async fn list(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, false).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match app.sieve.list(session.account_id()).await {
        Ok(v) => json(200, &ListJson { scripts: v.into_iter().map(|s| ScriptJson { name: s.name, active: s.active, size: s.size }).collect(), max_scripts: MAX_SCRIPTS, max_bytes: MAX_SCRIPT_BYTES }),
        Err(_) => error(503, "busy"),
    }
}

#[derive(Serialize)]
struct BodyJson {
    name: String,
    body: String,
}

async fn get(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, false).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(name) = name_of(&req) else { return error(404, "not_found") };
    match app.sieve.get(session.account_id(), &name).await {
        Ok(Some(body)) => json(200, &BodyJson { name, body }),
        Ok(None) => error(404, "not_found"),
        Err(_) => error(503, "busy"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutBody {
    body: String,
}

#[derive(Serialize)]
struct Refused {
    error: &'static str,
    line: usize,
    message: String,
}

async fn put(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(name) = name_of(&req) else { return error(404, "not_found") };
    let Ok(b) = serde_json::from_slice::<PutBody>(&req.body) else { return error(400, "invalid") };
    if b.body.len() > MAX_SCRIPT_BYTES {
        return error(413, "too_large");
    }
    if let Err(e) = rusty_sieve::parse::parse(&b.body) {
        return json(400, &Refused { error: "script_invalid", line: e.line, message: e.message });
    }
    match app.sieve.put(session.account_id(), &name, &b.body).await {
        Ok(()) => json(200, &serde_json::json!({})),
        Err(Error::Quota(_)) => error(429, "too_many"),
        Err(_) => error(503, "busy"),
    }
}

async fn activate(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(name) = name_of(&req) else { return error(404, "not_found") };
    match app.sieve.set_active(session.account_id(), Some(&name)).await {
        Ok(()) => json(200, &serde_json::json!({})),
        Err(Error::NotFound(_)) => error(404, "not_found"),
        Err(_) => error(503, "busy"),
    }
}

async fn deactivate(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match app.sieve.set_active(session.account_id(), None).await {
        Ok(()) => json(200, &serde_json::json!({})),
        Err(_) => error(503, "busy"),
    }
}

async fn remove(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(name) = name_of(&req) else { return error(404, "not_found") };
    match app.sieve.delete(session.account_id(), &name).await {
        Ok(true) => json(200, &serde_json::json!({})),
        Ok(false) => error(404, "not_found"),
        Err(Error::Conflict(_)) => error(409, "active"),
        Err(_) => error(503, "busy"),
    }
}

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
    let router = route!(router, Method::Get, "/api/v1/sieve", list);
    let router = route!(router, Method::Post, "/api/v1/sieve/deactivate", deactivate);
    let router = route!(router, Method::Get, "/api/v1/sieve/{name}", get);
    let router = route!(router, Method::Put, "/api/v1/sieve/{name}", put);
    let router = route!(router, Method::Post, "/api/v1/sieve/{name}/activate", activate);
    route!(router, Method::Delete, "/api/v1/sieve/{name}", remove)
}
