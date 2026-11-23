//! The routes of the second factor (docs/design-2fa.md): `GET /api/v1/totp` (is it offered, on, begun), `POST /api/v1/totp/begin` (the password again; answers the secret and the link, once), `POST /api/v1/totp/confirm`
//! (the first code; answers the recovery codes, once) and `POST /api/v1/totp/disable` (the password and a code).

use std::sync::Arc;

use rusty_core::Error;
use rusty_http::{handler, Method, Request, Response, Router};
use serde::{Deserialize, Serialize};

use crate::api::{error, SignInError, WebmailApp};
use crate::compose::now;

fn json<T: Serialize>(v: &T) -> Response {
    Response::json(200, v).with_header("cache-control", "no-store")
}

#[derive(Serialize)]
struct Status {
    /// This service can make and check codes (its key file is set).
    available: bool,
    enabled: bool,
    /// Begun and not yet confirmed.
    pending: bool,
}

async fn status(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, false).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let sf = app.auth.second();
    match (sf.enabled(session.account_id()).await, sf.pending(session.account_id()).await) {
        (Ok(enabled), Ok(pending)) => json(&Status { available: sf.available(), enabled, pending }),
        _ => error(503, "busy"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordBody {
    password: String,
}

#[derive(Serialize)]
struct Begun {
    /// The secret as text for an app that is typed into.
    secret: String,
    /// The `otpauth://` link for an app that opens it.
    uri: String,
}

/// The password again, throttled like a sign-in; the failure is the fixed answer.
async fn reconfirm(app: &WebmailApp, req: &Request, session: &crate::session::Session, password: &str) -> Result<(), Response> {
    match app.auth.confirm(req.peer.addr.ip(), &session.address, password).await {
        Ok(a) if a == session.account_id() => Ok(()),
        Ok(_) | Err(SignInError::Invalid | SignInError::SecondFactor) => Err(error(401, "unauthorized")),
        Err(SignInError::Throttled { retry_after_secs }) => Err(error(429, "throttled").with_header("retry-after", retry_after_secs.max(1).to_string())),
        Err(SignInError::Busy | SignInError::Unavailable) => Err(error(503, "busy")),
    }
}

async fn begin(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if !app.auth.second().available() {
        return error(503, "not_configured");
    }
    let Ok(b) = serde_json::from_slice::<PasswordBody>(&req.body) else { return error(400, "invalid") };
    if let Err(r) = reconfirm(app, &req, &session, &b.password).await {
        return r;
    }
    match app.auth.second().begin(session.account_id(), &session.address).await {
        Ok((secret, uri)) => json(&Begun { secret, uri }),
        Err(Error::Conflict(_)) => error(409, "already_on"),
        Err(_) => error(503, "busy"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeBody {
    code: String,
}

#[derive(Serialize)]
struct Recovery {
    /// Ten codes, each good once; shown only now.
    recovery: Vec<String>,
}

async fn confirm(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<CodeBody>(&req.body) else { return error(400, "invalid") };
    match app.auth.second().confirm(session.account_id(), &b.code, now().max(0) as u64).await {
        Ok(Some(recovery)) => json(&Recovery { recovery }),
        Ok(None) => error(401, "wrong_code"),
        Err(Error::Conflict(_)) => error(409, "already_on"),
        Err(_) => error(503, "busy"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DisableBody {
    password: String,
    code: String,
}

async fn disable(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<DisableBody>(&req.body) else { return error(400, "invalid") };
    if let Err(r) = reconfirm(app, &req, &session, &b.password).await {
        return r;
    }
    match app.auth.second().check(session.account_id(), &b.code, now().max(0) as u64).await {
        Ok(true) => {}
        Ok(false) => return error(401, "wrong_code"),
        Err(_) => return error(503, "busy"),
    }
    match app.auth.second().disable(session.account_id()).await {
        Ok(()) => json(&serde_json::json!({})),
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
    let router = route!(router, Method::Get, "/api/v1/totp", status);
    let router = route!(router, Method::Post, "/api/v1/totp/begin", begin);
    let router = route!(router, Method::Post, "/api/v1/totp/confirm", confirm);
    route!(router, Method::Post, "/api/v1/totp/disable", disable)
}
