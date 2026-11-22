//! Plans (docs/design-plans-quota.md): a named bundle of limits, `quota_bytes` for the mailbox and `max_message_bytes` for one message. Platform administrators define them; an administrator
//! puts an account of its own domain on one. The services read the limits through the directory layers (account, plan, domain, global config) when they open a mailbox, so a change
//! reaches the next delivery or session without a restart.

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::api::{fail, AdminApp};
use crate::manage::{platform_only, scoped_account};

/// The largest quota and message size an administrator may set: far above any real mailbox, below what a signed 64-bit count of bytes cannot hold.
const MAX_BYTES: u64 = 1 << 50;
const MAX_NAME: usize = 64;

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

#[derive(Serialize)]
struct PlanJson {
    name: String,
    quota_bytes: Option<u64>,
    max_message_bytes: Option<u64>,
    messages_per_hour: Option<u64>,
    /// The protocols accounts of the plan may use (`imap`, `pop3`, `submission`); `null`: all of them.
    protocols: Option<Vec<String>>,
    /// The spam score from which mail goes to the Spam folder, and from which it is refused; `null`: rspamd's own actions decide.
    spam_junk_score: Option<f64>,
    spam_reject_score: Option<f64>,
}

#[derive(Serialize)]
struct PlansJson {
    items: Vec<PlanJson>,
}

fn plan_json(name: &str, config: &str) -> PlanJson {
    let v: Value = serde_json::from_str(config).unwrap_or(Value::Null);
    PlanJson {
        name: name.to_owned(),
        quota_bytes: v.get("quota_bytes").and_then(Value::as_u64),
        max_message_bytes: v.get("max_message_bytes").and_then(Value::as_u64),
        messages_per_hour: v.get("messages_per_hour").and_then(Value::as_u64),
        protocols: v.get("protocols").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()),
        spam_junk_score: v.get("spam_junk_score").and_then(Value::as_f64),
        spam_reject_score: v.get("spam_reject_score").and_then(Value::as_f64),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutPlan {
    name: String,
    quota_bytes: Option<u64>,
    max_message_bytes: Option<u64>,
    #[serde(default)]
    messages_per_hour: Option<u64>,
    #[serde(default)]
    protocols: Option<Vec<String>>,
    #[serde(default)]
    spam_junk_score: Option<f64>,
    #[serde(default)]
    spam_reject_score: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutAccountPlan {
    plan: Option<String>,
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= MAX_NAME && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')) && !n.starts_with("__")
}

async fn list(app: &AdminApp, req: Request) -> Response {
    if let Err(r) = app.require(&req, false).await {
        return r;
    }
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    match s.plans.list().await {
        Ok(rows) => Response::json(200, &PlansJson { items: rows.iter().map(|p| plan_json(&p.name, &p.config)).collect() }).with_header("cache-control", "no-store"),
        Err(e) => fail(&e),
    }
}

/// Create the plan or change its limits (a `null` limit removes the key, so the plan has none of its own: the layers below decide).
/// The spam lines: each between 0 and 100, and the line for the Spam folder below the line for refusing.
fn spam_scores_ok(junk: Option<f64>, reject: Option<f64>) -> bool {
    let in_range = |x: f64| x.is_finite() && (0.0..=100.0).contains(&x);
    junk.is_none_or(in_range) && reject.is_none_or(in_range) && !matches!((junk, reject), (Some(j), Some(r)) if j >= r)
}

async fn put(app: &AdminApp, req: Request) -> Response {
    let (_session, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = platform_only(&p) {
        return r;
    }
    let Ok(b) = serde_json::from_slice::<PutPlan>(&req.body) else { return err(400, "invalid") };
    let protocols_ok = b.protocols.as_ref().is_none_or(|p| p.iter().all(|x| ["imap", "pop3", "submission"].contains(&x.as_str())));
    if !valid_name(&b.name)
        || b.quota_bytes.is_some_and(|q| q == 0 || q > MAX_BYTES)
        || b.max_message_bytes.is_some_and(|m| m == 0 || m > MAX_BYTES)
        || b.messages_per_hour.is_some_and(|m| m == 0 || m > 1_000_000)
        || !protocols_ok
        || !spam_scores_ok(b.spam_junk_score, b.spam_reject_score)
    {
        return err(400, "invalid");
    }
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    let existing = match s.plans.list().await {
        Ok(rows) => rows.into_iter().find(|r| r.name == b.name),
        Err(e) => return fail(&e),
    };
    let mut config: Map<String, Value> = existing.as_ref().and_then(|r| serde_json::from_str(&r.config).ok()).unwrap_or_default();
    for (key, value) in [("quota_bytes", b.quota_bytes), ("max_message_bytes", b.max_message_bytes), ("messages_per_hour", b.messages_per_hour)] {
        match value {
            Some(v) => {
                config.insert(key.to_owned(), json!(v));
            }
            None => {
                config.remove(key);
            }
        }
    }
    for (key, value) in [("spam_junk_score", b.spam_junk_score), ("spam_reject_score", b.spam_reject_score)] {
        match value {
            Some(v) => {
                config.insert(key.to_owned(), json!(v));
            }
            None => {
                config.remove(key);
            }
        }
    }
    match &b.protocols {
        Some(list) => {
            config.insert("protocols".to_owned(), json!(list));
        }
        None => {
            config.remove("protocols");
        }
    }
    let result = match &existing {
        Some(row) => app.directory.set_plan_config(rusty_core::PlanId(row.id), Value::Object(config.clone())).await,
        None => app.directory.create_plan(&b.name, Value::Object(config.clone())).await.map(|_| ()),
    };
    if let Err(e) = result {
        return fail(&e);
    }
    let status = if existing.is_some() { 200 } else { 201 };
    Response::json(status, &plan_json(&b.name, &Value::Object(config).to_string())).with_header("cache-control", "no-store")
}

/// Put an account on a plan, or on none (`"plan": null`).
async fn put_account_plan(app: &AdminApp, req: Request) -> Response {
    let (_session, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<PutAccountPlan>(&req.body) else { return err(400, "invalid") };
    if b.plan.as_deref().is_some_and(|n| !valid_name(n)) {
        return err(400, "invalid");
    }
    let (acc, _dom) = match scoped_account(app, &p, req.param("a").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    match s.plans.set_account_plan(acc.id, b.plan.as_deref()).await {
        Ok(()) => Response::json(200, &json!({ "plan": b.plan })).with_header("cache-control", "no-store"),
        Err(rusty_core::Error::NotFound(_)) => err(404, "not_found"),
        Err(e) => fail(&e),
    }
}

/// The routes of this part of the API.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    let (a1, a2, a3) = (app.clone(), app.clone(), app);
    router
        .route(Method::Get, "/api/v1/plans", handler(move |r: Request| {
            let a = a1.clone();
            async move { list(&a, r).await }
        }))
        .route(Method::Put, "/api/v1/plans", handler(move |r: Request| {
            let a = a2.clone();
            async move { put(&a, r).await }
        }))
        .route(Method::Put, "/api/v1/accounts/{a}/plan", handler(move |r: Request| {
            let a = a3.clone();
            async move { put_account_plan(&a, r).await }
        }))
}
