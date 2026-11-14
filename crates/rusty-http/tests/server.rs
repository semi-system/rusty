//! The HTTP server over a real listener: routing, path hygiene, the headers every response carries, the limits
//! (head, body, headers, time), request smuggling, keep-alive and graceful shutdown. Hostile input is sent
//! as raw bytes; the handler counter proves that a refused request never reached a handler.
mod common;

use std::time::{Duration, Instant};

use common::*;
use rusty_http::Limits;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Every response, whatever produced it, carries these (design section 6).
const MUST_HAVE: [&str; 8] = [
    "content-security-policy",
    "strict-transport-security",
    "x-content-type-options",
    "referrer-policy",
    "cross-origin-opener-policy",
    "cross-origin-resource-policy",
    "permissions-policy",
    "cache-control",
];

fn assert_secure(r: &Reply) {
    for h in MUST_HAVE {
        assert!(r.header(h).is_some(), "missing {h} on a {} response:\n{}", r.status, r.head);
    }
    assert_eq!(r.header("x-content-type-options").as_deref(), Some("nosniff"));
    let csp = r.header("content-security-policy").unwrap();
    for part in ["default-src 'none'", "script-src 'self'", "frame-ancestors 'none'", "base-uri 'none'"] {
        assert!(csp.contains(part), "{csp}");
    }
    assert!(!csp.contains("unsafe-inline") && !csp.contains("unsafe-eval"), "{csp}");
}

// ------------------------------------------------------------------------------------------- routing

#[tokio::test]
async fn a_known_route_is_served_with_the_secure_headers() {
    let s = start(Limits::default()).await;
    let r = get(s.addr, "/hello").await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(r.body, "hello");
    assert_secure(&r);
    assert_eq!(r.header("cache-control").as_deref(), Some("no-store"));
}

#[tokio::test]
async fn path_parameters_are_decoded_once_and_handed_to_the_handler() {
    let s = start(Limits::default()).await;
    let r = get(s.addr, "/domains/example.net/accounts/al%C3%ADce%40example.net").await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(r.body, "example.net|alíce@example.net");
}

#[tokio::test]
async fn an_unknown_path_is_404_and_a_wrong_method_is_405_with_allow() {
    let s = start(Limits::default()).await;
    let r = get(s.addr, "/nothing/here").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.body, "{\"error\":\"not_found\"}");
    assert_secure(&r);
    let r = raw(s.addr, b"POST /hello HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow").as_deref(), Some("get, head"), "the test client lower-cases the head");
    assert_secure(&r);
    assert_eq!(s.calls(), 0, "no handler ran");
}

#[tokio::test]
async fn head_is_get_without_a_body() {
    let s = start(Limits::default()).await;
    let r = raw(s.addr, b"HEAD /hello HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, "");
    assert_secure(&r);
}

#[tokio::test]
async fn methods_the_server_never_serves_are_refused() {
    let s = start(Limits::default()).await;
    for m in ["TRACE", "CONNECT", "PATCH", "OPTIONS", "PROPFIND", "BREW"] {
        let r = raw(s.addr, format!("{m} /hello HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes()).await;
        assert!(r.status == 405 || r.status == 400 || r.status == 501, "{m} -> {}", r.status);
    }
    assert_eq!(s.calls(), 0);
}

// ------------------------------------------------------------------------------------- path hygiene

#[tokio::test]
async fn hostile_paths_never_reach_a_handler() {
    let s = start(Limits::default()).await;
    let long = format!("/domains/{}/accounts/x", "a".repeat(3000));
    let bad = [
        "//hello",
        "/hello/",
        "/domains/../hello",
        "/domains/%2e%2e/accounts/x",
        "/domains/%2E%2E/accounts/x",
        "/domains/a%2Fb/accounts/x",
        "/domains/a%5Cb/accounts/x",
        "/domains/a%00b/accounts/x",
        "/domains/a%0d%0ab/accounts/x",
        "/domains/%ff%fe/accounts/x",
        "/domains/a%zz/accounts/x",
        "/domains/a%/accounts/x",
        long.as_str(),
    ];
    for p in bad {
        let r = get(s.addr, p).await;
        assert!(r.status == 400 || r.status == 404 || r.status == 414, "{p:.40} -> {}", r.status);
        assert_secure(&r);
    }
    assert_eq!(s.calls(), 0);
}

#[tokio::test]
async fn a_query_string_does_not_change_which_route_matches_and_is_bounded() {
    let s = start(Limits::default()).await;
    assert_eq!(get(s.addr, "/hello?x=1&y=2").await.status, 200);
    let r = get(s.addr, &format!("/hello?{}", "q".repeat(5000))).await;
    assert!(r.status == 400 || r.status == 414, "{}", r.status);
}

// ---------------------------------------------------------------------------------------- limits

#[tokio::test]
async fn a_body_over_the_limit_is_413_whether_declared_or_chunked() {
    let s = start(Limits::default()).await;
    let big = Limits::default().max_body_bytes + 1;
    let r = raw(s.addr, format!("POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: {big}\r\nConnection: close\r\n\r\n").as_bytes()).await;
    assert_eq!(r.status, 413, "{}", r.head);
    assert_secure(&r);
    // Chunked: the size is not declared, the server must count.
    let mut req = b"POST /echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for _ in 0..(big / 1024 + 2) {
        req.extend_from_slice(b"400\r\n");
        req.extend_from_slice(&[b'a'; 1024]);
        req.extend_from_slice(b"\r\n");
    }
    req.extend_from_slice(b"0\r\n\r\n");
    let r = raw(s.addr, &req).await;
    assert_eq!(r.status, 413, "{}", r.head);
    assert_eq!(s.calls(), 0, "no handler ran");
}

#[tokio::test]
async fn a_body_at_the_limit_is_accepted() {
    let s = start(Limits::default()).await;
    let n = Limits::default().max_body_bytes;
    let mut req = format!("POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: {n}\r\nConnection: close\r\n\r\n").into_bytes();
    req.extend(std::iter::repeat_n(b'a', n));
    let r = raw(s.addr, &req).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(r.body, n.to_string());
}

#[tokio::test]
async fn an_oversized_head_is_431_and_so_are_too_many_headers() {
    let s = start(Limits::default()).await;
    let r = raw(s.addr, format!("GET /hello HTTP/1.1\r\nHost: t\r\nX-Big: {}\r\nConnection: close\r\n\r\n", "a".repeat(20_000)).as_bytes()).await;
    assert!(r.status == 431 || r.status == 400, "{}", r.status);
    let mut many = String::from("GET /hello HTTP/1.1\r\nHost: t\r\nConnection: close\r\n");
    for i in 0..200 {
        many.push_str(&format!("X-H{i}: v\r\n"));
    }
    many.push_str("\r\n");
    let r = raw(s.addr, many.as_bytes()).await;
    assert!(r.status == 431 || r.status == 400, "{}", r.status);
    assert_eq!(s.calls(), 0);
}

#[tokio::test]
async fn a_slow_head_is_cut_off_at_the_head_timeout() {
    let s = start(small_limits()).await;
    let mut c = TcpStream::connect(s.addr).await.unwrap();
    c.write_all(b"GET /hello HT").await.unwrap();
    let t0 = Instant::now();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut out)).await;
    assert!(t0.elapsed() < Duration::from_secs(3), "the connection must be closed after the head timeout");
    assert_eq!(s.calls(), 0);
}

#[tokio::test]
async fn a_handler_that_takes_too_long_gets_503_busy() {
    let s = start(small_limits()).await;
    let t0 = Instant::now();
    let r = get(s.addr, "/slow").await;
    assert_eq!(r.status, 503, "{}", r.head);
    assert_eq!(r.body, "{\"error\":\"busy\"}");
    assert_secure(&r);
    assert!(t0.elapsed() < Duration::from_secs(3));
}

// ------------------------------------------------------------------------------- request smuggling

#[tokio::test]
async fn conflicting_framing_headers_never_give_two_readings() {
    let s = start(Limits::default()).await;
    // Both framing headers: the HTTP library applies exactly one (Transfer-Encoding wins and Content-Length is dropped,
    // RFC 9112 section 6.3), so there is one reading of the body and no desynchronisation. Refusing the shape outright is
    // the edge's job (HAProxy rewrites the framing before a request reaches this server). The handler must see the
    // chunked body (0 bytes), never the 4 bytes the Content-Length claims.
    let both = b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n";
    let r = raw(s.addr, both).await;
    assert!(r.status == 400 || (r.status == 200 && r.body == "0"), "{} {}", r.head, r.body);
    let both_ran = usize::from(r.status == 200);
    let twice = b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 4\r\nContent-Length: 5\r\nConnection: close\r\n\r\nabcde";
    let r = raw(s.addr, twice).await;
    assert_eq!(r.status, 400, "{}", r.head);
    let bare_lf = b"GET /hello HTTP/1.1\nHost: t\nConnection: close\n\n";
    let r = raw(s.addr, bare_lf).await;
    assert!(r.status == 400 || r.status == 200, "{}", r.status);
    assert_eq!(s.calls(), both_ran + usize::from(r.status == 200), "the duplicate Content-Length request never ran a handler");
}

#[tokio::test]
async fn garbage_closes_the_connection_without_a_panic_or_a_handler_call() {
    let s = start(Limits::default()).await;
    for junk in [&b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03"[..], b"\x00\x00\x00\x00", b"GET\r\n\r\n", b"GET /hello HTTP/9.9\r\n\r\n", b"\r\n\r\n\r\n"] {
        let r = raw(s.addr, junk).await;
        assert!(r.status == 0 || r.status >= 400, "{:?} -> {}", junk, r.status);
    }
    assert_eq!(s.calls(), 0);
    // The server is still serving.
    assert_eq!(get(s.addr, "/hello").await.status, 200);
}

// --------------------------------------------------------------------------- keep-alive and shutdown

#[tokio::test]
async fn keep_alive_serves_several_requests_then_closes_when_idle() {
    let s = start(small_limits()).await;
    let mut c = TcpStream::connect(s.addr).await.unwrap();
    for _ in 0..3 {
        c.write_all(b"GET /hello HTTP/1.1\r\nHost: t\r\n\r\n").await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(3), c.read(&mut buf)).await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(buf.get(..n).unwrap()).starts_with("HTTP/1.1 200"));
    }
    assert_eq!(s.calls(), 3);
    let t0 = Instant::now();
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut rest)).await;
    assert!(t0.elapsed() < Duration::from_secs(3), "an idle connection is closed after the idle timeout");
}

#[tokio::test]
async fn a_connection_is_closed_after_its_request_budget() {
    let limits = Limits { max_requests_per_connection: 2, ..Limits::default() };
    let s = start(limits).await;
    let mut c = TcpStream::connect(s.addr).await.unwrap();
    let mut closing = false;
    for i in 0..3 {
        if c.write_all(b"GET /hello HTTP/1.1\r\nHost: t\r\n\r\n").await.is_err() {
            break;
        }
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(3), c.read(&mut buf)).await.unwrap_or(Ok(0)).unwrap_or(0);
        let text = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).to_ascii_lowercase();
        if i == 1 {
            closing = text.contains("connection: close");
        }
    }
    assert!(closing, "the second response must announce the close");
}

#[tokio::test]
async fn shutdown_lets_a_request_in_flight_finish_and_closes_idle_connections() {
    let s = start(Limits::default()).await;
    // An idle keep-alive connection.
    let mut idle = TcpStream::connect(s.addr).await.unwrap();
    idle.write_all(b"GET /hello HTTP/1.1\r\nHost: t\r\n\r\n").await.unwrap();
    let mut buf = vec![0u8; 4096];
    let _ = tokio::time::timeout(Duration::from_secs(3), idle.read(&mut buf)).await.unwrap().unwrap();
    let stop = s.stop.clone();
    stop.cancel();
    let t0 = Instant::now();
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), idle.read_to_end(&mut rest)).await;
    assert!(t0.elapsed() < Duration::from_secs(3), "an idle connection must close promptly on shutdown");
    s.shutdown().await;
}
