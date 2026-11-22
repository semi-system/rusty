//! The mail rules of one account (docs/design-mail-rules.md): forwarding and the automatic reply. An administrator of the account's domain sets them here; the webmail will give the person the
//! same two calls for their own account. They are the key `mail_rules` of the account's own configuration, read by the inbound service at delivery.

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use rusty_smtp::rules::{MailRules, KEY};
use serde::Serialize;
use serde_json::Value;

use crate::api::{fail, AdminApp};
use crate::manage::scoped_account;

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

#[derive(Serialize)]
struct ForwardJson {
    to: Vec<String>,
    keep: bool,
}

#[derive(Serialize)]
struct VacationJson {
    enabled: bool,
    subject: String,
    body: String,
    from: Option<i64>,
    until: Option<i64>,
}

#[derive(Serialize)]
struct RulesJson {
    forward: Option<ForwardJson>,
    vacation: Option<VacationJson>,
}

fn rules_json(r: &MailRules) -> RulesJson {
    RulesJson {
        forward: r.forward.as_ref().map(|f| ForwardJson { to: f.to.clone(), keep: f.keep }),
        vacation: r.vacation.as_ref().map(|v| VacationJson { enabled: v.enabled, subject: v.subject.clone(), body: v.body.clone(), from: v.from, until: v.until }),
    }
}

async fn get(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let (acc, _) = match scoped_account(app, &p, req.param("a").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match app.directory.account_config(acc.id).await {
        Ok(c) => Response::json(200, &rules_json(&c.get(KEY).map(MailRules::from_value).unwrap_or_default())).with_header("cache-control", "no-store"),
        Err(e) => fail(&e),
    }
}

async fn put(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let (acc, _) = match scoped_account(app, &p, req.param("a").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Ok(body) = serde_json::from_slice::<Value>(&req.body) else { return err(400, "invalid") };
    let rules = match MailRules::validate(&body) {
        Ok(r) => r,
        Err(_) => return err(400, "invalid"),
    };
    let mut config = match app.directory.account_config(acc.id).await {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let stored = rules.to_value();
    if let Some(map) = config.as_object_mut() {
        if stored.as_object().is_some_and(|o| o.is_empty()) {
            map.remove(KEY);
        } else {
            map.insert(KEY.to_owned(), stored);
        }
    }
    if let Err(e) = app.directory.set_account_config(acc.id, config).await {
        return fail(&e);
    }
    Response::json(200, &rules_json(&rules)).with_header("cache-control", "no-store")
}

pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    let a = app.clone();
    let router = router.route(Method::Get, "/api/v1/accounts/{a}/mail-rules", handler(move |r: Request| {
        let a = a.clone();
        async move { get(&a, r).await }
    }));
    router.route(Method::Put, "/api/v1/accounts/{a}/mail-rules", handler(move |r: Request| {
        let a = app.clone();
        async move { put(&a, r).await }
    }))
}
