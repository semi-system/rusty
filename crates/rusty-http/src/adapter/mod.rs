//! The only module that names the third-party HTTP library. Everything it takes or returns is Rusty-owned;
//! nothing here is `pub`.

use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use rusty_svc::{BoxedStream, PeerInfo};
use tokio_util::sync::CancellationToken;

use crate::request::{Headers, Method, Params, Request};
use crate::response::Response;
use crate::server::Core;

/// Serve one connection until it ends, or until `shutdown` is cancelled (then the request in flight finishes,
/// an idle connection closes at once, and nothing new is read).
pub(crate) async fn serve(stream: BoxedStream, peer: PeerInfo, shutdown: CancellationToken, core: Arc<Core>) {
    let limits = core.limits.clone();
    let served = Arc::new(AtomicU32::new(0));
    let svc_core = core.clone();
    let service = service_fn(move |req: hyper::Request<Incoming>| {
        let (core, served) = (svc_core.clone(), served.clone());
        async move { Ok::<_, Infallible>(respond(&core, peer, &served, req).await) }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(limits.head_timeout)
        .max_buf_size(limits.max_head_bytes)
        .max_headers(limits.max_headers)
        .keep_alive(true)
        .half_close(false);
    let conn = builder.serve_connection(TokioIo::new(stream), service);
    tokio::pin!(conn);
    tokio::select! {
        res = conn.as_mut() => {
            if let Err(e) = res {
                tracing::debug!(error = %e, "http connection ended with an error");
            }
        }
        _ = shutdown.cancelled() => {
            conn.as_mut().graceful_shutdown();
            let _ = tokio::time::timeout(limits.request_timeout, conn).await;
        }
    }
}

async fn respond(core: &Core, peer: PeerInfo, served: &AtomicU32, req: hyper::Request<Incoming>) -> hyper::Response<Full<Bytes>> {
    let n = served.fetch_add(1, Ordering::SeqCst).saturating_add(1);
    let (resp, force_close) = build(core, peer, req).await;
    to_hyper(core, resp, force_close || n >= core.limits.max_requests_per_connection)
}

fn method_of(m: &hyper::Method) -> Option<Method> {
    match m.as_str() {
        "GET" => Some(Method::Get),
        "HEAD" => Some(Method::Head),
        "POST" => Some(Method::Post),
        "PUT" => Some(Method::Put),
        "DELETE" => Some(Method::Delete),
        _ => None,
    }
}

/// The response for one request, and whether the connection must be closed after it.
async fn build(core: &Core, peer: PeerInfo, req: hyper::Request<Incoming>) -> (Response, bool) {
    let (parts, body) = req.into_parts();
    let Some(method) = method_of(&parts.method) else { return (core.error(405, "method_not_allowed"), true) };
    let path = parts.uri.path();
    let query = parts.uri.query();
    let target_len = path.len().saturating_add(query.map_or(0, |q| q.len().saturating_add(1)));
    if path.len() > core.limits.max_path_bytes || target_len > core.limits.max_target_bytes {
        return (core.error(414, "invalid"), true);
    }
    // Routing happens before the body is read: a refused request costs nothing.
    let (handler, params) = match core.refusal(core.find(method, path)) {
        Ok(x) => x,
        Err(resp) => return (resp, true),
    };
    let mut headers = Headers::default();
    for (name, value) in &parts.headers {
        if let Ok(v) = value.to_str() {
            headers.push(name.as_str().to_owned(), v.to_owned());
        }
    }
    let max = core.limits.max_body_bytes;
    let declared = headers.get("content-length").and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max as u64) {
        return (core.error(413, "too_large"), true);
    }
    let body = match Limited::new(body, max).collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return (core.error(413, "too_large"), true),
    };
    let request = Request { method, path: path.to_owned(), query: query.map(str::to_owned), headers, body, peer, params: Params(params.0) };
    (core.run(handler, request).await, false)
}

fn to_hyper(core: &Core, resp: Response, close: bool) -> hyper::Response<Full<Bytes>> {
    match try_to_hyper(&resp, close) {
        Some(r) => r,
        None => {
            // A handler produced something that is not valid HTTP (a bad header, an impossible status): the
            // client gets a plain 500, never a half-built response.
            tracing::error!("a handler returned an invalid response");
            let fallback = core.error(500, "internal");
            try_to_hyper(&fallback, true).unwrap_or_else(|| hyper::Response::new(Full::new(Bytes::new())))
        }
    }
}

fn try_to_hyper(resp: &Response, close: bool) -> Option<hyper::Response<Full<Bytes>>> {
    let mut b = hyper::Response::builder().status(hyper::StatusCode::from_u16(resp.status).ok()?);
    for (name, value) in &resp.headers {
        b = b.header(HeaderName::from_bytes(name.as_bytes()).ok()?, HeaderValue::from_str(value).ok()?);
    }
    if close {
        b = b.header(hyper::header::CONNECTION, "close");
    }
    b.body(Full::new(resp.body.clone())).ok()
}
