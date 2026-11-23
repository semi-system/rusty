//! The person's own settings of the webmail: for now the signature a new message starts with. `GET /api/v1/settings` and `PUT /api/v1/settings` (`{"signature": ".."}`); the text is plain, at most 2000 characters,
//! and never markup (the composer puts it into the text of a message).

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::MAX_SIGNATURE;
use serde::{Deserialize, Serialize};

use crate::api::{error, WebmailApp};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsJson {
    signature: String,
}

fn json(v: &SettingsJson) -> Response {
    Response::json(200, v).with_header("cache-control", "no-store")
}

async fn get(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, false).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match app.settings.signature(session.account_id()).await {
        Ok(signature) => json(&SettingsJson { signature }),
        Err(_) => error(503, "busy"),
    }
}

async fn put(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<SettingsJson>(&req.body) else { return error(400, "invalid") };
    if b.signature.chars().count() > MAX_SIGNATURE || b.signature.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        return error(400, "invalid");
    }
    // Line ends are kept as LF; the composer turns them into CRLF when it builds a message.
    let signature = b.signature.replace("\r\n", "\n");
    match app.settings.set_signature(session.account_id(), &signature).await {
        Ok(()) => json(&SettingsJson { signature }),
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
    let router = route!(router, Method::Get, "/api/v1/settings", get);
    route!(router, Method::Put, "/api/v1/settings", put)
}
