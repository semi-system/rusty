//! Cookies and the request guards of the admin API (design section 5): the `__Host-` session cookie, and the
//! three separate checks on a state-changing request (`Origin`, JSON content type, CSRF token). Pure: requests are
//! built by hand, so every refusal is tested on its own.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use rusty_http::cookie::{clear_cookie, parse_cookies, session_cookie};
use rusty_http::guard::{csrf_ok, guard_mutation, is_json, origin_ok};
use rusty_http::{Headers, Method, Params, Request};
use rusty_svc::PeerInfo;

const ORIGIN: &str = "https://admin.example.net";
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn req(method: Method, headers: &[(&str, &str)]) -> Request {
    let h = Headers::from_pairs(headers.iter().copied());
    let addr: SocketAddr = "192.0.2.1:50000".parse().unwrap();
    Request { method, path: "/api/v1/domains".into(), query: None, headers: h, body: Bytes::new(), peer: PeerInfo { addr, tls: true }, params: Params::default() }
}

// ----------------------------------------------------------------------------------------- cookies

#[test]
fn a_cookie_header_is_split_into_names_and_values() {
    assert_eq!(parse_cookies("a=b; c=d;e=f"), vec![("a", "b"), ("c", "d"), ("e", "f")]);
    assert_eq!(parse_cookies("__Host-rusty_admin=abc123"), vec![("__Host-rusty_admin", "abc123")]);
    assert!(parse_cookies("").is_empty());
}

#[test]
fn malformed_pairs_are_skipped_and_the_rest_survives() {
    assert_eq!(parse_cookies("novalue; a=b; =x; c d=e; f=g\u{7f}h; i=j"), vec![("a", "b"), ("i", "j")]);
    // A quoted value, a space, a comma, a semicolon or a control character is not a cookie octet.
    assert!(parse_cookies("a=\"b\"").is_empty());
    assert!(parse_cookies("a=b c").is_empty());
}

#[test]
fn the_cookie_header_is_bounded() {
    let many: String = (0..100).map(|i| format!("k{i}=v")).collect::<Vec<_>>().join("; ");
    assert!(parse_cookies(&many).len() <= 32, "at most 32 pairs are read");
    assert!(parse_cookies(&format!("a={}", "x".repeat(5000))).is_empty(), "a header over 4096 bytes is not read");
}

#[test]
fn the_session_cookie_has_every_protective_attribute() {
    let c = session_cookie("__Host-rusty_admin", "AbC-_.~123", Duration::from_secs(1800)).unwrap();
    assert_eq!(c, "__Host-rusty_admin=AbC-_.~123; Max-Age=1800; Path=/; Secure; HttpOnly; SameSite=Strict");
    assert!(!c.to_ascii_lowercase().contains("domain"), "a __Host- cookie never has a Domain");
}

#[test]
fn a_cookie_that_would_break_the_header_or_the_host_rule_is_refused() {
    let ok = Duration::from_secs(60);
    assert!(session_cookie("rusty_admin", "v", ok).is_none(), "the name must carry the __Host- prefix");
    assert!(session_cookie("__Host-", "v", ok).is_none());
    assert!(session_cookie("__Host-a b", "v", ok).is_none());
    assert!(session_cookie("__Host-a", "", ok).is_none());
    for bad in ["a;b", "a b", "a,b", "a\r\nSet-Cookie: x=y", "a\"b", "é", "a\\b"] {
        assert!(session_cookie("__Host-a", bad, ok).is_none(), "{bad:?}");
    }
    assert!(session_cookie("__Host-a", &"x".repeat(257), ok).is_none());
    assert!(session_cookie("__Host-a", "v", Duration::ZERO).is_none(), "a zero lifetime is clear_cookie");
}

#[test]
fn clearing_a_cookie_keeps_the_same_attributes() {
    let c = clear_cookie("__Host-rusty_admin").unwrap();
    assert_eq!(c, "__Host-rusty_admin=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Strict");
    assert!(clear_cookie("other").is_none());
}

#[test]
fn a_request_cookie_is_read_only_when_it_is_unambiguous() {
    let r = req(Method::Get, &[("cookie", "a=1; __Host-s=tok; b=2")]);
    assert_eq!(r.cookie("__Host-s"), Some("tok"));
    assert_eq!(r.cookie("missing"), None);
    // Two cookies of one name (a planted second one) are refused, not guessed between.
    let r = req(Method::Get, &[("cookie", "__Host-s=one; __Host-s=two")]);
    assert_eq!(r.cookie("__Host-s"), None);
    // A second Cookie header is read as part of the same set (HTTP/2 style), and a duplicate across them is refused too.
    let r = req(Method::Get, &[("cookie", "a=1"), ("cookie", "a=2")]);
    assert_eq!(r.cookie("a"), None);
}

// ------------------------------------------------------------------------------------------ guards

#[test]
fn origin_must_equal_the_public_origin_on_a_mutating_request() {
    for m in [Method::Post, Method::Put, Method::Delete] {
        assert!(origin_ok(&req(m, &[("origin", ORIGIN)]), ORIGIN));
        assert!(!origin_ok(&req(m, &[]), ORIGIN), "a missing Origin is refused");
        assert!(!origin_ok(&req(m, &[("origin", "null")]), ORIGIN));
        assert!(!origin_ok(&req(m, &[("origin", "https://evil.example")]), ORIGIN));
        assert!(!origin_ok(&req(m, &[("origin", "https://admin.example.net.evil.example")]), ORIGIN));
        assert!(!origin_ok(&req(m, &[("origin", "http://admin.example.net")]), ORIGIN), "the scheme counts");
        assert!(!origin_ok(&req(m, &[("origin", "https://admin.example.net:8443")]), ORIGIN), "the port counts");
        assert!(!origin_ok(&req(m, &[("origin", "https://ADMIN.example.net")]), ORIGIN), "the comparison is exact");
        assert!(!origin_ok(&req(m, &[("origin", ORIGIN), ("origin", ORIGIN)]), ORIGIN), "two Origin headers are refused");
    }
    // Safe methods are not state-changing.
    assert!(origin_ok(&req(Method::Get, &[]), ORIGIN));
    assert!(origin_ok(&req(Method::Head, &[("origin", "https://evil.example")]), ORIGIN));
}

#[test]
fn only_json_is_accepted_as_a_request_body_type() {
    assert!(is_json(&req(Method::Post, &[("content-type", "application/json")])));
    assert!(is_json(&req(Method::Post, &[("content-type", "application/json; charset=utf-8")])));
    assert!(is_json(&req(Method::Post, &[("content-type", "Application/JSON")])));
    for bad in ["text/plain", "application/x-www-form-urlencoded", "multipart/form-data; boundary=x", "application/jsonp", "application/json2", "text/json", ""] {
        assert!(!is_json(&req(Method::Post, &[("content-type", bad)])), "{bad:?}");
    }
    assert!(!is_json(&req(Method::Post, &[])), "a missing content type is refused");
    assert!(!is_json(&req(Method::Post, &[("content-type", "application/json"), ("content-type", "text/plain")])));
}

#[test]
fn the_csrf_token_must_match_exactly() {
    assert!(csrf_ok(&req(Method::Post, &[("x-csrf-token", TOKEN)]), TOKEN));
    assert!(!csrf_ok(&req(Method::Post, &[]), TOKEN));
    assert!(!csrf_ok(&req(Method::Post, &[("x-csrf-token", "")]), TOKEN));
    assert!(!csrf_ok(&req(Method::Post, &[("x-csrf-token", &TOKEN[..63])]), TOKEN));
    assert!(!csrf_ok(&req(Method::Post, &[("x-csrf-token", &format!("{TOKEN}0"))]), TOKEN));
    let mut flipped = TOKEN.to_owned();
    flipped.replace_range(63..64, "0");
    assert!(!csrf_ok(&req(Method::Post, &[("x-csrf-token", &flipped)]), TOKEN));
    assert!(!csrf_ok(&req(Method::Post, &[("x-csrf-token", TOKEN), ("x-csrf-token", TOKEN)]), TOKEN), "two headers are refused");
    assert!(!csrf_ok(&req(Method::Post, &[("x-csrf-token", TOKEN)]), ""), "an empty session token never matches");
}

#[test]
fn the_three_guards_each_refuse_on_their_own_with_a_fixed_code() {
    let good = [("origin", ORIGIN), ("content-type", "application/json"), ("x-csrf-token", TOKEN)];
    assert!(guard_mutation(&req(Method::Post, &good), ORIGIN, TOKEN).is_ok());
    // Safe methods pass without any of them.
    assert!(guard_mutation(&req(Method::Get, &[]), ORIGIN, TOKEN).is_ok());
    let without = |skip: &str| -> Vec<(&str, &str)> { good.iter().copied().filter(|(n, _)| *n != skip).collect() };
    let r = guard_mutation(&req(Method::Post, &without("origin")), ORIGIN, TOKEN).unwrap_err();
    assert_eq!((r.status, r.body.as_ref()), (403, &b"{\"error\":\"forbidden\"}"[..]));
    let r = guard_mutation(&req(Method::Post, &without("x-csrf-token")), ORIGIN, TOKEN).unwrap_err();
    assert_eq!((r.status, r.body.as_ref()), (403, &b"{\"error\":\"forbidden\"}"[..]));
    let r = guard_mutation(&req(Method::Post, &without("content-type")), ORIGIN, TOKEN).unwrap_err();
    assert_eq!((r.status, r.body.as_ref()), (415, &b"{\"error\":\"unsupported\"}"[..]));
}
