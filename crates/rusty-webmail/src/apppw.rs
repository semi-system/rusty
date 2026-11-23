//! App passwords (docs/design-app-passwords.md): the signed-in person makes a password for one device or program (a phone's mail app, a script), sees the list, and revokes one by one. The mail protocols
//! accept them besides the account's own password; the webmail and the admin web never do. A new one needs the account's password again, is shown once, and only its hash is kept.

use std::sync::Arc;

use rand_core::{OsRng, RngCore};
use rusty_core::Error;
use rusty_directory::hash_app_password;
use rusty_http::{handler, Method, Request, Response, Router};
use serde::{Deserialize, Serialize};

use crate::api::{error, SignInError, WebmailApp};

const MAX_LABEL: usize = 60;
/// Letters and digits that cannot be mistaken for one another when read from a screen.
const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

fn json<T: Serialize>(status: u16, v: &T) -> Response {
    Response::json(status, v).with_header("cache-control", "no-store")
}

/// Sixteen characters from [`ALPHABET`] in four groups (about 79 bits): `abcd-efgh-jkmn-pqrs`.
fn generate() -> String {
    let mut raw = [0u8; 16];
    // Rejection sampling: no modulo bias.
    let limit = 256 - (256 % ALPHABET.len());
    let mut out = String::with_capacity(19);
    let mut n = 0;
    while n < 16 {
        OsRng.fill_bytes(&mut raw);
        for b in raw {
            if usize::from(b) < limit && n < 16 {
                if n > 0 && n % 4 == 0 {
                    out.push('-');
                }
                out.push(char::from(ALPHABET[usize::from(b) % ALPHABET.len()]));
                n += 1;
            }
        }
    }
    out
}

#[derive(Serialize)]
struct ItemJson {
    id: i64,
    label: String,
    created: i64,
    last_used: Option<i64>,
}

#[derive(Serialize)]
struct ListJson {
    items: Vec<ItemJson>,
}

async fn list(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, false).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match app.app_passwords.list(session.account_id()).await {
        Ok(items) => json(200, &ListJson { items: items.into_iter().map(|i| ItemJson { id: i.id, label: i.label, created: i.created_at, last_used: i.last_used_at }).collect() }),
        Err(_) => error(503, "busy"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    label: String,
    /// The account's own password, again.
    password: String,
}

#[derive(Serialize)]
struct Created {
    id: i64,
    label: String,
    /// The password itself: shown this once.
    secret: String,
}

async fn create(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<CreateBody>(&req.body) else { return error(400, "invalid") };
    let label = b.label.trim();
    if label.is_empty() || label.chars().count() > MAX_LABEL || label.chars().any(char::is_control) {
        return error(400, "invalid");
    }
    match app.auth.confirm(req.peer.addr.ip(), &session.address, &b.password).await {
        Ok(a) if a == session.account_id() => {}
        Ok(_) | Err(SignInError::Invalid | SignInError::SecondFactor) => return error(401, "unauthorized"),
        Err(SignInError::Throttled { retry_after_secs }) => return error(429, "throttled").with_header("retry-after", retry_after_secs.max(1).to_string()),
        Err(SignInError::Busy | SignInError::Unavailable) => return error(503, "busy"),
    }
    let secret = generate();
    let Ok(hash) = hash_app_password(&secret).await else { return error(503, "busy") };
    match app.app_passwords.create(session.account_id(), label, &hash).await {
        Ok(id) => json(200, &Created { id, label: label.to_owned(), secret }),
        Err(Error::Quota(_)) => error(429, "too_many"),
        Err(_) => error(503, "busy"),
    }
}

async fn revoke(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(id) = req.param("id").and_then(|i| i.parse::<i64>().ok()).filter(|n| *n > 0) else { return error(404, "not_found") };
    match app.app_passwords.revoke(session.account_id(), id).await {
        Ok(true) => json(200, &serde_json::json!({})),
        Ok(false) => error(404, "not_found"),
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
    let router = route!(router, Method::Get, "/api/v1/app-passwords", list);
    let router = route!(router, Method::Post, "/api/v1/app-passwords", create);
    route!(router, Method::Delete, "/api/v1/app-passwords/{id}", revoke)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_password_is_four_groups_of_four_from_the_alphabet_and_differs_each_time() {
        let a = generate();
        assert_eq!(a.len(), 19);
        assert!(a.split('-').all(|g| g.len() == 4 && g.bytes().all(|b| ALPHABET.contains(&b))));
        assert_ne!(a, generate());
    }
}
