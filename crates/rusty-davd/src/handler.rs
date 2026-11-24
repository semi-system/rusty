//! The routes of the service: every path of `/dav/` for every method of WebDAV goes to one handler that signs the client in and hands the request to [`rusty_dav::server::Dav`]. `OPTIONS` needs no
//! sign-in (clients ask what is there before they send a password); every other request without credentials is answered `401` with the challenge. The well-known paths of CalDAV and CardDAV
//! (RFC 6764) redirect to `/dav/`.

use std::sync::Arc;

use rusty_dav::server::{Dav, Reply, Req, Verb};
use rusty_http::{handler, Method, Request, Response, Router};

use crate::auth::{DavAuth, Refused};

pub struct DavApp {
    pub dav: Dav,
    pub auth: DavAuth,
}

const CHALLENGE: &str = "Basic realm=\"Rusty\", charset=\"UTF-8\"";

/// The paths that name a resource, with the parameters the router wants (the handler reads the raw path itself).
const PATTERNS: [&str; 9] = [
    "/dav",
    "/dav/principals/{a}",
    "/dav/calendars/{a}",
    "/dav/addressbooks/{a}",
    "/dav/calendars/{a}/{c}",
    "/dav/addressbooks/{a}/{c}",
    "/dav/calendars/{a}/{c}/{o}",
    "/dav/addressbooks/{a}/{c}/{o}",
    "/dav/principals",
];

const METHODS: [(Method, Verb); 11] = [
    (Method::Options, Verb::Options),
    (Method::Propfind, Verb::Propfind),
    (Method::Proppatch, Verb::Proppatch),
    (Method::Report, Verb::Report),
    (Method::Mkcol, Verb::Mkcol),
    (Method::Mkcalendar, Verb::Mkcalendar),
    (Method::Get, Verb::Get),
    (Method::Put, Verb::Put),
    (Method::Delete, Verb::Delete),
    (Method::Copy, Verb::Copy),
    (Method::Move, Verb::Move),
];

fn convert(reply: Reply) -> Response {
    let mut r = Response::new(reply.status).with_header("content-type", reply.content_type);
    for (n, v) in reply.headers {
        r = r.with_header(n, v);
    }
    r.with_body(reply.body.into_bytes())
}

fn unauthorized(why: &Refused) -> Response {
    match why {
        Refused::Credentials => Response::text(401, "sign in with your address and your password (or an app password)\n").with_header("www-authenticate", CHALLENGE),
        Refused::Throttled(secs) => Response::text(429, "too many failed sign-ins: wait a moment\n").with_header("retry-after", secs.to_string()),
        Refused::Busy => Response::text(503, "busy: try again in a moment\n").with_header("retry-after", "5"),
    }
}

async fn serve(app: Arc<DavApp>, verb: Verb, req: Request) -> Response {
    // What a client asks before it has a password.
    if verb == Verb::Options {
        return convert(Dav::options_of(&req.path));
    }
    let who = match app.auth.check(req.peer.addr.ip(), req.headers.get("authorization")).await {
        Ok(p) => p,
        Err(why) => return unauthorized(&why),
    };
    let r = Req {
        verb,
        path: &req.path,
        depth: req.headers.get("depth"),
        if_match: req.headers.get("if-match"),
        if_none_match: req.headers.get("if-none-match"),
        destination: req.headers.get("destination"),
        overwrite: req.headers.get("overwrite"),
        body: &req.body,
    };
    convert(app.dav.handle(&who, &r).await)
}

pub fn routes(app: Arc<DavApp>, router: Router) -> Router {
    let mut router = router;
    for pattern in PATTERNS {
        for (method, verb) in METHODS {
            let app = app.clone();
            router = router.route(method, pattern, handler(move |req: Request| serve(app.clone(), verb, req)));
        }
    }
    // The addresses a client finds from the domain name (RFC 6764): they send it on to the service root.
    for well_known in ["/.well-known/caldav", "/.well-known/carddav"] {
        for method in [Method::Get, Method::Propfind, Method::Options] {
            router = router.route(method, well_known, handler(|_req: Request| async { Response::new(301).with_header("location", "/dav/") }));
        }
    }
    router
}
