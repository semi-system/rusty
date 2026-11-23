//! A platform administrator's search of the mail of every mailbox (docs/design-admin-search.md): who, which folder, when, from whom, the subject and the size of what matched, never a body.
//!
//! The admin service has no access to mail, so a search goes through the database: `POST /api/v1/search` writes the request (and, like every change of the admin web, the audit log keeps who asked and
//! what for), the IMAP service runs it where the mailboxes are, and `GET /api/v1/search/{id}` reads what it found. It is **off** unless the platform setting `admin_search` is on (a privacy risk that
//! the owner of the platform has to choose), and for platform administrators only.

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::{Search, SearchCriteria};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{fail, AdminApp};

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ask {
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    until: Option<i64>,
}

/// A text to look for: not empty, at most 200 characters, no control character.
fn text_ok(s: &Option<String>) -> bool {
    s.as_deref().is_none_or(|t| !t.trim().is_empty() && t.chars().count() <= 200 && !t.chars().any(char::is_control))
}

fn criteria(a: Ask) -> Option<SearchCriteria> {
    let trimmed = |s: Option<String>| s.map(|t| t.trim().to_owned());
    let c = SearchCriteria { domain: trimmed(a.domain).map(|d| d.to_ascii_lowercase()), account: trimmed(a.account).map(|d| d.to_ascii_lowercase()), from_text: trimmed(a.from), to_text: trimmed(a.to), subject_text: trimmed(a.subject), since: a.since, until: a.until };
    let something = c.from_text.is_some() || c.to_text.is_some() || c.subject_text.is_some() || c.since.is_some() || c.until.is_some();
    let ok = something
        && [&c.from_text, &c.to_text, &c.subject_text].into_iter().all(text_ok)
        && c.domain.as_deref().is_none_or(|d| !d.is_empty() && d.len() <= 253 && !d.contains('@'))
        && c.account.as_deref().is_none_or(|a| a.len() <= 320 && a.contains('@'))
        && c.since.is_none_or(|s| s >= 0)
        && c.until.is_none_or(|u| u >= 0)
        && !matches!((c.since, c.until), (Some(s), Some(u)) if s > u);
    ok.then_some(c)
}

#[derive(Serialize)]
struct SearchJson {
    id: i64,
    state: String,
    found: i64,
    truncated: bool,
    results: Value,
    error: Option<String>,
    created_at: i64,
    finished_at: Option<i64>,
}

fn search_json(s: Search) -> SearchJson {
    SearchJson { id: s.id, state: s.state, found: s.found, truncated: s.truncated, results: serde_json::from_str(&s.results).unwrap_or_else(|_| Value::Array(Vec::new())), error: s.error, created_at: s.created_at, finished_at: s.finished_at }
}

/// Is the search on? The platform setting `admin_search` in the global configuration.
async fn enabled(app: &AdminApp) -> Result<bool, Response> {
    let Some(s) = app.services.as_ref() else { return Err(err(503, "unavailable")) };
    match s.plans.global_config().await {
        Ok(c) => Ok(serde_json::from_str::<Value>(&c).ok().and_then(|v| v.get("admin_search").and_then(Value::as_bool)).unwrap_or(false)),
        Err(e) => Err(fail(&e)),
    }
}

async fn ask(app: &AdminApp, req: Request) -> Response {
    let (session, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if !p.scope.is_platform() {
        return err(403, "forbidden");
    }
    match enabled(app).await {
        Ok(true) => {}
        Ok(false) => return err(409, "wrong_state"),
        Err(r) => return r,
    }
    let Ok(a) = serde_json::from_slice::<Ask>(&req.body) else { return err(400, "invalid") };
    let Some(c) = criteria(a) else { return err(400, "invalid") };
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    match s.searches.create(&session.address, &c).await {
        Ok(id) => Response::json(201, &serde_json::json!({ "id": id, "state": "queued" })).with_header("cache-control", "no-store"),
        Err(e) => fail(&e),
    }
}

async fn read(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if !p.scope.is_platform() {
        return err(403, "forbidden");
    }
    let Some(id) = req.param("s").and_then(|s| s.parse::<i64>().ok()).filter(|n| *n > 0) else { return err(404, "not_found") };
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    match s.searches.get(id).await {
        Ok(Some(found)) => Response::json(200, &search_json(found)).with_header("cache-control", "no-store"),
        Ok(None) => err(404, "not_found"),
        Err(e) => fail(&e),
    }
}

pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    let a = app.clone();
    let router = router.route(Method::Post, "/api/v1/search", handler(move |req| {
        let app = a.clone();
        async move { ask(&app, req).await }
    }));
    router.route(Method::Get, "/api/v1/search/{s}", handler(move |req| {
        let app = app.clone();
        async move { read(&app, req).await }
    }))
}
