//! Entry points shared by the cargo-fuzz targets (`fuzz/fuzz_targets/fuzz_http_*.rs`) and their stable-toolchain
//! proptest stand-ins (`tests/fuzz_smoke.rs`): each takes arbitrary bytes, must never panic, and asserts the
//! invariants of the layer it drives.

use std::net::SocketAddr;

use bytes::Bytes;
use rusty_svc::PeerInfo;

use crate::cookie::{clear_cookie, parse_cookies, session_cookie};
use crate::guard::{csrf_ok, guard_mutation, is_json, origin_ok};
use crate::request::{Headers, Method, Params, Request};
use crate::response::Response;
use crate::router::{check_path, handler, Router, Routed};

/// The path check and the router: an accepted path has only well-formed segments, and a route is chosen only for
/// an accepted path.
pub fn check_request_path(data: &[u8]) {
    let Ok(path) = std::str::from_utf8(data) else { return };
    let accepted = check_path(path);
    if let Some(segments) = &accepted {
        for (raw, decoded) in segments {
            assert!(!raw.is_empty() && !decoded.is_empty());
            assert!(!raw.contains('/'), "a raw segment holds no slash");
            assert!(decoded != "." && decoded != "..");
            assert!(!decoded.chars().any(|c| c.is_control() || c == '/' || c == '\\'), "{decoded:?}");
        }
        assert!(path.starts_with('/'));
    }
    let h = handler(|_r: Request| async { Response::new(200) });
    let router = Router::new().route(Method::Get, "/a/{x}/b/{y}", h.clone()).route(Method::Post, "/a/{x}", h).fallback(handler(|_r: Request| async { Response::new(200) }));
    for m in [Method::Get, Method::Head, Method::Post, Method::Put, Method::Delete] {
        match router.find(m, path) {
            Routed::BadPath => assert!(accepted.is_none()),
            Routed::Found { params, .. } => {
                assert!(accepted.is_some(), "a route was chosen for a refused path");
                for (_, v) in &params.0 {
                    assert!(!v.is_empty() && !v.contains('/') && !v.contains('\\'));
                }
            }
            Routed::NotFound | Routed::MethodNotAllowed { .. } => assert!(accepted.is_some()),
        }
    }
}

/// The cookie reader and builder: bounded, well-formed, and a built cookie reads back as itself.
pub fn check_cookies(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let pairs = parse_cookies(text);
    assert!(pairs.len() <= 32);
    for (n, v) in &pairs {
        assert!(!n.is_empty() && n.bytes().all(|b| b.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?={} ".contains(&b)));
        assert!(v.bytes().all(|b| (0x21..0x7f).contains(&b) && !b" \",;\\".contains(&b)));
    }
    // A cookie built from the same text is either refused or reads back as the same pair after the attributes are cut.
    let name = format!("__Host-{}", text.chars().take(12).collect::<String>());
    if let Some(set) = session_cookie(&name, text, std::time::Duration::from_secs(60)) {
        assert!(!set.contains('\r') && !set.contains('\n'));
        let first = set.split(';').next().unwrap_or_default();
        assert_eq!(parse_cookies(first), vec![(name.as_str(), text)]);
        assert!(set.ends_with("Path=/; Secure; HttpOnly; SameSite=Strict"));
        assert!(!set.to_ascii_lowercase().contains("domain"));
    }
    if let Some(c) = clear_cookie(&name) {
        assert!(c.contains("Max-Age=0"));
    }
}

/// The request guards over headers built from the input (one `name: value` per line): none panics, and a request the
/// combined guard lets through passes each guard on its own.
pub fn check_guards(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let pairs: Vec<(&str, &str)> = text.lines().filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim(), v.trim())).take(64).collect();
    let addr: SocketAddr = SocketAddr::from(([192, 0, 2, 1], 50000));
    let origin = "https://admin.example.net";
    let token = "0123456789abcdef0123456789abcdef";
    for method in [Method::Get, Method::Post, Method::Delete] {
        let req = Request {
            method,
            path: "/api/v1/x".into(),
            query: None,
            headers: Headers::from_pairs(pairs.iter().copied()),
            body: Bytes::new(),
            peer: PeerInfo { addr, tls: true },
            params: Params::default(),
        };
        let _ = (req.cookie("__Host-rusty_admin"), is_json(&req));
        if guard_mutation(&req, origin, token).is_ok() && method != Method::Get {
            assert!(origin_ok(&req, origin) && csrf_ok(&req, token) && is_json(&req));
        }
    }
}
