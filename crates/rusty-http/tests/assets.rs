//! The embedded front end: static files from memory with cache rules, conditional requests and the single-page
//! fallback, and the rule that an API path never turns into the index page.
mod common;

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use common::*;
use rusty_http::assets::{Asset, Assets};
use rusty_http::{Limits, Router};

const INDEX: &[u8] = b"<!doctype html><title>app</title><script src=\"/assets/app.3f2a1c.js\"></script>";
const JS: &[u8] = b"console.log(1)";
const CSS: &[u8] = b"body{margin:0}";

fn assets() -> Assets {
    Assets::new(vec![
        Asset::new("/index.html", INDEX).unwrap(),
        Asset::new("/assets/app.3f2a1c.js", JS).unwrap(),
        Asset::new("/assets/app.3f2a1c.css", CSS).unwrap(),
    ])
}

async fn serve() -> Server {
    let router = Router::new().fallback(assets().handler());
    start_router(router, Limits::default(), Arc::new(AtomicUsize::new(0))).await
}

#[test]
fn an_asset_needs_a_checked_path_and_a_known_type() {
    assert!(Asset::new("/a/b.js", b"x").is_some());
    for bad in ["a.js", "/a//b.js", "/../a.js", "/a/%2e%2e/b.js", "/a b.js", "/a.php", "/a", "/a.exe", "/.env", "/a.js/"] {
        assert!(Asset::new(bad, b"x").is_none(), "{bad:?}");
    }
}

#[tokio::test]
async fn the_index_page_is_served_at_the_root_and_is_never_cached_blindly() {
    let s = serve().await;
    let r = get(s.addr, "/").await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(r.body.as_bytes(), INDEX);
    assert_eq!(r.header("content-type").as_deref(), Some("text/html; charset=utf-8"));
    assert_eq!(r.header("cache-control").as_deref(), Some("no-cache"));
    assert!(r.header("etag").is_some());
    let csp = r.header("content-security-policy").unwrap();
    assert!(csp.contains("script-src 'self'") && !csp.contains("unsafe-inline"), "{csp}");
    assert_eq!(get(s.addr, "/index.html").await.body.as_bytes(), INDEX);
}

#[tokio::test]
async fn hashed_files_are_immutable_and_have_the_right_type() {
    let s = serve().await;
    let js = get(s.addr, "/assets/app.3f2a1c.js").await;
    assert_eq!(js.status, 200);
    assert_eq!(js.body.as_bytes(), JS);
    assert_eq!(js.header("content-type").as_deref(), Some("text/javascript; charset=utf-8"));
    assert_eq!(js.header("cache-control").as_deref(), Some("public, max-age=31536000, immutable"));
    assert_eq!(js.header("x-content-type-options").as_deref(), Some("nosniff"));
    let css = get(s.addr, "/assets/app.3f2a1c.css").await;
    assert_eq!(css.header("content-type").as_deref(), Some("text/css; charset=utf-8"));
}

#[tokio::test]
async fn a_conditional_request_gets_304_without_a_body() {
    let s = serve().await;
    let first = get(s.addr, "/assets/app.3f2a1c.js").await;
    let etag = first.header("etag").unwrap();
    for cond in [etag.as_str(), "*", &format!("\"nope\", {etag}")] {
        let r = raw(s.addr, format!("GET /assets/app.3f2a1c.js HTTP/1.1\r\nHost: t\r\nIf-None-Match: {cond}\r\nConnection: close\r\n\r\n").as_bytes()).await;
        assert_eq!(r.status, 304, "{cond}: {}", r.head);
        assert_eq!(r.body, "");
        assert_eq!(r.header("etag").as_deref(), Some(etag.as_str()));
    }
    let r = raw(s.addr, b"GET /assets/app.3f2a1c.js HTTP/1.1\r\nHost: t\r\nIf-None-Match: \"other\"\r\nConnection: close\r\n\r\n").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn head_has_the_headers_and_no_body() {
    let s = serve().await;
    let r = raw(s.addr, b"HEAD /assets/app.3f2a1c.js HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, "");
    assert!(r.header("etag").is_some());
}

#[tokio::test]
async fn an_application_route_gets_the_index_page_but_an_api_path_never_does() {
    let s = serve().await;
    let r = get(s.addr, "/domains/example.net/dns").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body.as_bytes(), INDEX, "a client-side route is the index page");
    assert_eq!(r.header("cache-control").as_deref(), Some("no-cache"));
    for p in ["/api/v1/domains", "/api", "/api/v1/nothing/here"] {
        let r = get(s.addr, p).await;
        assert_eq!(r.status, 404, "{p}");
        assert_eq!(r.body, "{\"error\":\"not_found\"}", "{p}");
    }
}

#[tokio::test]
async fn a_missing_file_is_404_not_the_index_page() {
    let s = serve().await;
    for p in ["/assets/missing.js", "/robots.txt", "/x.php", "/assets/app.3f2a1c.js.map"] {
        assert_eq!(get(s.addr, p).await.status, 404, "{p}");
    }
}

#[tokio::test]
async fn traversal_never_gets_as_far_as_the_files() {
    let s = serve().await;
    for p in ["/assets/..%2f..%2fetc%2fpasswd", "/assets/../index.html", "/%2e%2e/%2e%2e/etc/passwd", "/assets//app.3f2a1c.js"] {
        let r = get(s.addr, p).await;
        assert_eq!(r.status, 400, "{p}");
    }
}

#[tokio::test]
async fn nothing_but_get_and_head_is_served_from_the_files() {
    let s = serve().await;
    for m in ["POST", "PUT", "DELETE"] {
        let r = raw(s.addr, format!("{m} /index.html HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await;
        assert!(r.status == 404 || r.status == 405, "{m} -> {}", r.status);
        assert!(!r.body.contains("<title>"));
    }
}
