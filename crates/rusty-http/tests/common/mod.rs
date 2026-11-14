#![allow(dead_code)]
//! A real listener (`BoundListener` over plain TCP, the same serving loop the mail services use) with the
//! HTTP handler, and a raw TCP client so that malformed and hostile requests can be sent byte by byte.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rusty_http::{handler, HttpConfig, HttpHandler, Limits, Method, Request, Response, Router};
use rusty_svc::{BoundListener, DirectTcp, ListenerSpec, TracingEvents};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

pub struct Server {
    pub addr: SocketAddr,
    pub stop: CancellationToken,
    pub calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

impl Server {
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Cancel, then wait for the serving loop to finish (the drain).
    pub async fn shutdown(self) {
        self.stop.cancel();
        let Server { task, .. } = &self;
        let _ = tokio::time::timeout(Duration::from_secs(10), async { while !task.is_finished() { tokio::time::sleep(Duration::from_millis(10)).await } }).await;
    }
}

pub fn small_limits() -> Limits {
    Limits { head_timeout: Duration::from_millis(400), request_timeout: Duration::from_millis(600), ..Limits::default() }
}

/// The routes of the tests: an echo of the parameters, a body counter and a slow one.
pub fn router(calls: Arc<AtomicUsize>) -> Router {
    let c1 = calls.clone();
    let c2 = calls.clone();
    let c3 = calls.clone();
    let c4 = calls;
    Router::new()
        .route(Method::Get, "/hello", handler(move |_req: Request| { let c = c1.clone(); async move { c.fetch_add(1, Ordering::SeqCst); Response::text(200, "hello") } }))
        .route(Method::Get, "/domains/{d}/accounts/{a}", handler(move |req: Request| { let c = c2.clone(); async move {
            c.fetch_add(1, Ordering::SeqCst);
            Response::text(200, &format!("{}|{}", req.param("d").unwrap_or("-"), req.param("a").unwrap_or("-")))
        } }))
        .route(Method::Post, "/echo", handler(move |req: Request| { let c = c3.clone(); async move {
            c.fetch_add(1, Ordering::SeqCst);
            Response::text(200, &format!("{}", req.body.len()))
        } }))
        .route(Method::Get, "/slow", handler(move |_req: Request| { let c = c4.clone(); async move {
            c.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(5)).await;
            Response::text(200, "late")
        } }))
}

pub async fn start(limits: Limits) -> Server {
    let calls = Arc::new(AtomicUsize::new(0));
    let h = HttpHandler::new(router(calls.clone()), HttpConfig { limits });
    let spec = ListenerSpec { name: "http-test", addr: "127.0.0.1:0".parse().unwrap(), max_connections: 64, max_per_ip: 64 };
    let bound = BoundListener::bind(spec).await.unwrap();
    let addr = bound.local_addr();
    let stop = CancellationToken::new();
    let s = stop.clone();
    let task = tokio::spawn(async move {
        let _ = bound.serve(Arc::new(DirectTcp), Arc::new(h), Arc::new(TracingEvents), s, Duration::from_secs(5)).await;
    });
    Server { addr, stop, calls, task }
}

/// What came back: the status code, the raw head (lower-cased) and the body.
pub struct Reply {
    pub status: u16,
    pub head: String,
    pub body: String,
    pub raw: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<String> {
        let needle = format!("\r\n{}:", name.to_ascii_lowercase());
        let i = self.head.find(&needle)?;
        let rest = self.head.get(i + needle.len()..)?;
        Some(rest.split("\r\n").next().unwrap_or_default().trim().to_owned())
    }
}

fn parse(raw: Vec<u8>) -> Reply {
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    Reply { status, head: format!("{}\r\n", head.to_ascii_lowercase()), body: body.to_owned(), raw }
}

/// Send `bytes` and read until the server closes (requests carry `Connection: close`).
pub async fn raw(addr: SocketAddr, bytes: &[u8]) -> Reply {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let _ = s.write_all(bytes).await;
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(8), s.read_to_end(&mut out)).await;
    parse(out)
}

pub async fn get(addr: SocketAddr, path: &str) -> Reply {
    raw(addr, format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes()).await
}
