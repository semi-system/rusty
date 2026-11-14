//! Routing on a path that was checked first. The checks come before any route is looked at, so a request
//! the server refuses never reaches a handler and a route table cannot be tricked by the spelling of a path.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;

use crate::request::{Method, Params, Request};
use crate::response::Response;

#[async_trait]
pub trait Handler: Send + Sync {
    async fn call(&self, req: Request) -> Response;
}

struct FnHandler<F>(F);

#[async_trait]
impl<F, Fut> Handler for FnHandler<F>
where
    F: Fn(Request) -> Fut + Send + Sync,
    Fut: Future<Output = Response> + Send,
{
    async fn call(&self, req: Request) -> Response {
        (self.0)(req).await
    }
}

/// A handler from an async closure or function.
pub fn handler<F, Fut>(f: F) -> Arc<dyn Handler>
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    Arc::new(FnHandler(f))
}

enum Seg {
    Lit(String),
    Param(String),
}

struct Route {
    method: Method,
    segs: Vec<Seg>,
    handler: Arc<dyn Handler>,
}

/// What the router decided about one request.
pub(crate) enum Routed {
    Found { handler: Arc<dyn Handler>, params: Params },
    NotFound,
    MethodNotAllowed { allow: Vec<Method> },
    /// The path is not acceptable at all (see [`check_path`]).
    BadPath,
}

#[derive(Default)]
pub struct Router {
    routes: Vec<Route>,
}

impl Router {
    pub fn new() -> Router {
        Router::default()
    }

    /// Add a route. `pattern` is `/literal/{param}/...`: no trailing slash, no empty segment. A bad pattern is a
    /// programming error and stops the process at start-up. `HEAD` is served by the `GET` route.
    pub fn route(mut self, method: Method, pattern: &str, handler: Arc<dyn Handler>) -> Router {
        assert!(method != Method::Head, "HEAD is served by the GET route");
        let segs = match split(pattern) {
            Some(s) => s,
            None => panic!("route pattern {pattern:?} is not a valid path"),
        };
        let segs = segs
            .into_iter()
            .map(|s| match s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
                Some(name) if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => Seg::Param(name.to_owned()),
                Some(_) => panic!("route pattern {pattern:?} has a bad parameter name"),
                None => Seg::Lit(s.to_owned()),
            })
            .collect();
        self.routes.push(Route { method, segs, handler });
        self
    }

    pub(crate) fn find(&self, method: Method, path: &str) -> Routed {
        let Some(raw) = check_path(path) else { return Routed::BadPath };
        let lookup = if method == Method::Head { Method::Get } else { method };
        let mut allow: Vec<Method> = Vec::new();
        for r in &self.routes {
            if r.segs.len() != raw.len() {
                continue;
            }
            let mut params = Vec::new();
            let mut ok = true;
            for (seg, (raw_seg, decoded)) in r.segs.iter().zip(raw.iter()) {
                match seg {
                    // Literals are compared as sent: a path spelt with escapes does not match.
                    Seg::Lit(l) => {
                        if l != raw_seg {
                            ok = false;
                            break;
                        }
                    }
                    Seg::Param(name) => params.push((name.clone(), decoded.clone())),
                }
            }
            if !ok {
                continue;
            }
            if r.method == lookup {
                return Routed::Found { handler: r.handler.clone(), params: Params(params) };
            }
            if !allow.contains(&r.method) {
                allow.push(r.method);
            }
        }
        if allow.is_empty() {
            return Routed::NotFound;
        }
        if allow.contains(&Method::Get) {
            allow.push(Method::Head);
        }
        let order = |m: &Method| match m {
            Method::Get => 0,
            Method::Head => 1,
            Method::Post => 2,
            Method::Put => 3,
            Method::Delete => 4,
        };
        allow.sort_by_key(order);
        Routed::MethodNotAllowed { allow }
    }
}

/// `/a/b` to `["a", "b"]`, `/` to `[]`; `None` for anything else (no leading slash, an empty segment).
fn split(path: &str) -> Option<Vec<&str>> {
    let rest = path.strip_prefix('/')?;
    if rest.is_empty() {
        return Some(Vec::new());
    }
    let segs: Vec<&str> = rest.split('/').collect();
    if segs.iter().any(|s| s.is_empty()) {
        return None;
    }
    Some(segs)
}

/// The decoded form of one path segment, or `None` when it is not acceptable: bad or incomplete escape, not
/// UTF-8, a control character, a slash or backslash (encoded or not), or a dot segment.
fn decode_segment(raw: &str) -> Option<String> {
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut it = raw.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hi = hex(it.next()?)?;
            let lo = hex(it.next()?)?;
            out.push(hi << 4 | lo);
        } else if b > 0x20 && b < 0x7f {
            out.push(b);
        } else {
            return None;
        }
    }
    let s = String::from_utf8(out).ok()?;
    if s == "." || s == ".." || s.chars().any(|c| c.is_control() || c == '/' || c == '\\') {
        return None;
    }
    Some(s)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Every segment of `path` as `(as sent, decoded)`, or `None` when the path is not acceptable.
pub(crate) fn check_path(path: &str) -> Option<Vec<(String, String)>> {
    split(path)?.into_iter().map(|s| decode_segment(s).map(|d| (s.to_owned(), d))).collect()
}
