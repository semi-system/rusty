//! The protocol switches (docs/design-protocols.md): which optional protocols the mail services answer. Platform administrators read and set them; the services read the table (cached
//! for a few seconds), so a change reaches every replica without a restart. The auto-setup answers (Thunderbird, Outlook, Apple) list a protocol only while it is on.

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::PROTOCOLS;
use serde::{Deserialize, Serialize};

use crate::api::{fail, AdminApp};
use crate::manage::platform_only;

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

#[derive(Serialize)]
struct ProtocolJson {
    name: &'static str,
    enabled: bool,
}

#[derive(Serialize)]
struct ProtocolsJson {
    protocols: Vec<ProtocolJson>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetBody {
    protocol: String,
    enabled: bool,
}

/// Every optional protocol with its state; one that was never set is on.
async fn state(app: &AdminApp) -> Result<ProtocolsJson, Response> {
    let Some(s) = app.services.as_ref() else { return Err(err(503, "unavailable")) };
    let stored = s.settings.protocols().await.map_err(|e| fail(&e))?;
    let protocols = PROTOCOLS.iter().map(|name| ProtocolJson { name, enabled: stored.iter().find(|(k, _)| k == name).is_none_or(|(_, on)| *on) }).collect();
    Ok(ProtocolsJson { protocols })
}

/// Whether one protocol is on, for the auto-setup answers: a store that cannot answer counts as on (the protocol is on by default and the answer is only a hint).
pub(crate) async fn is_on(app: &AdminApp, protocol: &str) -> bool {
    match state(app).await {
        Ok(s) => s.protocols.iter().find(|p| p.name == protocol).is_none_or(|p| p.enabled),
        Err(_) => true,
    }
}

async fn get(app: &AdminApp, req: Request) -> Response {
    let (_session, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = platform_only(&p) {
        return r;
    }
    match state(app).await {
        Ok(s) => Response::json(200, &s).with_header("cache-control", "no-store"),
        Err(r) => r,
    }
}

async fn put(app: &AdminApp, req: Request) -> Response {
    let (_session, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = platform_only(&p) {
        return r;
    }
    let Ok(b) = serde_json::from_slice::<SetBody>(&req.body) else { return err(400, "invalid") };
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    if let Err(e) = s.settings.set_protocol(&b.protocol, b.enabled, None).await {
        return fail(&e);
    }
    match state(app).await {
        Ok(s) => Response::json(200, &s).with_header("cache-control", "no-store"),
        Err(r) => r,
    }
}

/// The routes of this part of the API.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    let a = app.clone();
    let router = router.route(Method::Get, "/api/v1/protocols", handler(move |r: Request| {
        let a = a.clone();
        async move { get(&a, r).await }
    }));
    router.route(Method::Put, "/api/v1/protocols", handler(move |r: Request| {
        let a = app.clone();
        async move { put(&a, r).await }
    }))
}
