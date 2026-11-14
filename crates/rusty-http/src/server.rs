//! The server core: what is decided about a request independent of the HTTP library (the library's side is
//! `adapter`). Routing, the handler's time limit and the headers every response carries live here.

use std::sync::Arc;

use async_trait::async_trait;
use rusty_svc::{BoxedStream, PeerInfo, SessionHandler};
use tokio_util::sync::CancellationToken;

use crate::config::{HttpConfig, Limits, SECURITY_HEADERS};
use crate::request::{Method, Request};
use crate::response::Response;
use crate::router::{Handler, Router, Routed};

pub(crate) struct Core {
    router: Router,
    pub(crate) limits: Limits,
}

impl Core {
    /// A fixed error response (`{"error":"<code>"}`) with the headers every response carries.
    pub(crate) fn error(&self, status: u16, code: &'static str) -> Response {
        self.finish(Response::error(status, code), false)
    }

    pub(crate) fn find(&self, method: Method, path: &str) -> Routed {
        self.router.find(method, path)
    }

    /// The error response for what the router refused, or the handler to run.
    pub(crate) fn refusal(&self, routed: Routed) -> Result<(Arc<dyn Handler>, crate::request::Params), Response> {
        match routed {
            Routed::Found { handler, params } => Ok((handler, params)),
            Routed::BadPath => Err(self.error(400, "invalid")),
            Routed::NotFound => Err(self.error(404, "not_found")),
            Routed::MethodNotAllowed { allow } => {
                let list = allow.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(", ");
                Err(self.finish(Response::error(405, "method_not_allowed").with_header("allow", list), false))
            }
        }
    }

    /// Run the handler within the request timeout (`503 busy` when it takes longer).
    pub(crate) async fn run(&self, handler: Arc<dyn Handler>, req: Request) -> Response {
        let head_only = req.method == Method::Head;
        match tokio::time::timeout(self.limits.request_timeout, handler.call(req)).await {
            Ok(resp) => self.finish(resp, head_only),
            Err(_) => {
                tracing::warn!("a handler exceeded the request timeout");
                self.error(503, "busy")
            }
        }
    }

    /// Add the security headers the response lacks; a response to `HEAD` has no body.
    pub(crate) fn finish(&self, mut resp: Response, head_only: bool) -> Response {
        for (name, value) in SECURITY_HEADERS {
            if resp.header(name).is_none() {
                resp = resp.with_header(name, value);
            }
        }
        if head_only {
            resp.body = bytes::Bytes::new();
        }
        resp
    }
}

/// The HTTP service as a session handler: put it behind a `BoundListener` with the `Accept` of your choice
/// (plain TCP behind the edge, or implicit TLS), like every other listener of the platform.
pub struct HttpHandler {
    core: Arc<Core>,
}

impl HttpHandler {
    pub fn new(router: Router, config: HttpConfig) -> HttpHandler {
        HttpHandler { core: Arc::new(Core { router, limits: config.limits }) }
    }
}

#[async_trait]
impl SessionHandler for HttpHandler {
    async fn handle(&self, stream: BoxedStream, peer: PeerInfo, shutdown: CancellationToken) {
        crate::adapter::serve(stream, peer, shutdown, self.core.clone()).await;
    }
}
