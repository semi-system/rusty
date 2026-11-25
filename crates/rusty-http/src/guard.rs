//! The checks on a state-changing request (design section 5). `SameSite=Strict` on the session cookie is the first
//! layer; these are the second, and each can refuse on its own: the `Origin` must be the public origin, the body type
//! must be JSON (a form post or a text/plain request cannot be sent cross-site without a pre-flight the server never
//! grants), and the `X-CSRF-Token` must be the session's token.

use crate::request::{Method, Request};
use crate::response::Response;

fn mutating(m: Method) -> bool {
    matches!(m, Method::Post | Method::Put | Method::Delete | Method::Proppatch | Method::Mkcol | Method::Mkcalendar | Method::Copy | Method::Move)
}

/// A state-changing request must carry exactly one `Origin` header equal to `public_origin` (`https://host[:port]`,
/// compared exactly: scheme, host and port). `null`, a missing header and a lookalike are refused. Safe methods pass.
pub fn origin_ok(req: &Request, public_origin: &str) -> bool {
    if !mutating(req.method) {
        return true;
    }
    req.headers.count("origin") == 1 && req.headers.get("origin") == Some(public_origin)
}

/// The origin a request is to be compared with: the one of `extra` it carries in `Origin`, else `public_origin` (so a request from anywhere else fails as before).
pub fn pick_origin<'a>(req: &Request, public_origin: &'a str, extra: &'a [String]) -> &'a str {
    match req.headers.get("origin") {
        Some(o) if o != public_origin => extra.iter().map(String::as_str).find(|e| *e == o).unwrap_or(public_origin),
        _ => public_origin,
    }
}

/// Exactly one `Content-Type`, `application/json`, with no parameter other than `charset=utf-8`.
pub fn is_json(req: &Request) -> bool {
    if req.headers.count("content-type") != 1 {
        return false;
    }
    let Some(value) = req.headers.get("content-type") else { return false };
    let mut parts = value.split(';');
    let essence = parts.next().unwrap_or_default().trim();
    if !essence.eq_ignore_ascii_case("application/json") {
        return false;
    }
    parts.all(|p| {
        let p = p.trim();
        p.is_empty() || p.eq_ignore_ascii_case("charset=utf-8")
    })
}

/// Equality that does not stop at the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Exactly one `X-CSRF-Token` header, equal to the session's `token` (which is never empty).
pub fn csrf_ok(req: &Request, token: &str) -> bool {
    if token.is_empty() || req.headers.count("x-csrf-token") != 1 {
        return false;
    }
    req.headers.get("x-csrf-token").is_some_and(|v| same(v.as_bytes(), token.as_bytes()))
}

/// The three checks together for a state-changing request: `403 forbidden` for a bad origin or token, `415 unsupported`
/// for a body type that is not JSON. Safe methods pass without any of them.
pub fn guard_mutation(req: &Request, public_origin: &str, session_token: &str) -> Result<(), Response> {
    if !mutating(req.method) {
        return Ok(());
    }
    // Which of the two checks refused is said in a header (`x-refused: origin` or `csrf`), never with a value: the body stays the same for both, and a person or an operator who sees a 403 can tell a wrong Origin
    // (a host name the service is not configured for) from a token that is missing or is not the session's (a stale page).
    if !origin_ok(req, public_origin) {
        return Err(Response::error(403, "forbidden").with_header("x-refused", "origin"));
    }
    if !csrf_ok(req, session_token) {
        return Err(Response::error(403, "forbidden").with_header("x-refused", "csrf"));
    }
    if !is_json(req) {
        return Err(Response::error(415, "unsupported"));
    }
    Ok(())
}
