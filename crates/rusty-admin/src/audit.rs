//! The audit log of the admin web (docs/design-audit.md): every change an administrator makes (a request that is not a read and was not refused) is written with who, when, from which address
//! and what, kept for 365 days, and shown on the Audit page. A platform administrator sees every change; a domain administrator sees those of its own domain.
//!
//! The recording is one wrapper over the route table (`record_changes`), so a new route is audited without its author remembering to do it. The body is kept only as a summary of its fields with every secret hidden (`audit_detail`).

use std::sync::Arc;

use rusty_http::{handler, Handler, Method, Request, Response, Router};
use rusty_store::{AuditRow, NewAuditRow};
use serde::Serialize;

use crate::api::{fail, AdminApp};
use crate::auth::Scope;

const PAGE: usize = 50;
/// The session routes are not changes of the platform (a sign-in has no actor yet).
const SKIPPED: &str = "/api/v1/session";

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

/// Wrap every route that changes something so that its success is written to the audit log. A failed write is logged and does not undo the change (the change has happened).
pub fn record_changes(app: Arc<AdminApp>, router: Router) -> Router {
    router.wrap_routes(move |method, pattern, inner: Arc<dyn Handler>| {
        if method == Method::Get || method == Method::Head || pattern == SKIPPED || pattern == crate::transfers::CHUNK_ROUTE {
            return inner;
        }
        let (app, pattern) = (app.clone(), pattern.to_owned());
        handler(move |req: Request| {
            let (app, inner, pattern) = (app.clone(), inner.clone(), pattern.clone());
            async move {
                // Who it is, before the handler runs (it may end the session).
                let actor = match (req.cookie(crate::api::COOKIE), app.services.as_ref()) {
                    (Some(id), Some(_)) => app.auth.principal(id).await.ok().flatten(),
                    _ => None,
                };
                let detail = crate::audit_detail::summarize(&req.body);
                let (method, path, ip) = (req.method, req.path.clone(), req.peer.addr.ip().to_string());
                let domain_name = req.param("d").map(str::to_owned).or_else(|| req.param("a").and_then(|a| a.rsplit_once('@').map(|(_, d)| d.to_owned())));
                let resp = inner.call(req).await;
                if resp.status < 400 {
                    if let (Some((session, principal)), Some(services)) = (actor, app.services.as_ref()) {
                        let domain_id = match &domain_name {
                            Some(n) => app.directory.find_domain(n).await.ok().flatten().map(|d| d.id.0),
                            None => match principal.scope {
                                Scope::Domain(d) => Some(d.0),
                                Scope::Platform => None,
                            },
                        };
                        let row = NewAuditRow { actor_id: Some(session.account), actor: session.address.clone(), domain_id, ip, method: method.as_str().to_owned(), route: pattern, path, status: i32::from(resp.status), detail };
                        if let Err(e) = services.audit.record(&row).await {
                            tracing::warn!(error = %e, "the audit log could not be written");
                        }
                    }
                }
                resp
            }
        })
    })
}

#[derive(Serialize)]
struct EntryJson {
    id: i64,
    at: i64,
    actor: String,
    domain_id: Option<i64>,
    ip: String,
    method: String,
    route: String,
    path: String,
    status: i32,
    /// The fields that were set, as a JSON object of text values (secrets hidden), or `null`.
    detail: Option<String>,
}

#[derive(Serialize)]
struct AuditJson {
    items: Vec<EntryJson>,
    /// The `before=` of the next page, or `null` at the end.
    next: Option<i64>,
}

fn entry(r: AuditRow) -> EntryJson {
    EntryJson { id: r.id, at: r.at, actor: r.actor, domain_id: r.domain_id, ip: r.ip, method: r.method, route: r.route, path: r.path, status: r.status, detail: r.detail }
}

/// `GET /api/v1/audit?before=<id>&domain=<name>`: the newest changes first. A domain administrator always gets its own domain (`domain` must be absent or its own).
async fn list(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(services) = app.services.as_ref() else { return err(503, "busy") };
    let (mut before, mut domain) = (None, None);
    for part in req.query.as_deref().unwrap_or_default().split('&').filter(|x| !x.is_empty()) {
        match part.split_once('=') {
            Some(("before", v)) => match v.parse::<i64>() {
                Ok(n) => before = Some(n),
                Err(_) => return err(400, "invalid"),
            },
            Some(("domain", v)) if !v.is_empty() && v.len() <= 253 => domain = Some(v.to_owned()),
            _ => return err(400, "invalid"),
        }
    }
    let mut filter = match p.scope {
        Scope::Domain(d) => Some(d.0),
        Scope::Platform => None,
    };
    if let Some(name) = domain {
        let found = match app.directory.find_domain(&name).await {
            Ok(f) => f,
            Err(rusty_core::Error::Invalid(_)) => None,
            Err(e) => return fail(&e),
        };
        match found {
            Some(d) if p.scope.allows(d.id) => filter = Some(d.id.0),
            _ => return err(404, "not_found"),
        }
    }
    let mut rows = match services.audit.list(filter, before, i64::try_from(PAGE + 1).unwrap_or(51)).await {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let next = (rows.len() > PAGE).then(|| rows.get(PAGE - 1).map(|r| r.id)).flatten();
    rows.truncate(PAGE);
    Response::json(200, &AuditJson { items: rows.into_iter().map(entry).collect(), next }).with_header("cache-control", "no-store")
}

pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    router.route(Method::Get, "/api/v1/audit", handler(move |req| {
        let app = app.clone();
        async move { list(&app, req).await }
    }))
}
