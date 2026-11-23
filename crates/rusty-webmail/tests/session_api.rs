//! The webmail's session and sign-in (docs/design-webmail.md section 4): one fixed error for every way to fail, the Origin, JSON and CSRF checks, the throttle, an account that is disabled meanwhile, and the
//! health check. The service is started the way the binary starts it (`rusty_webmail::run::start`) against a real Postgres.

use std::net::SocketAddr;
use std::time::Duration;

use rusty_core::config::{Role, StartupConfig};
use rusty_directory::{AccountStatus, Directory};
use rusty_store::fixtures::pg_db as pg;
use rusty_store::{Store, StoreConfig};
use rusty_webmail::run::start;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

const ORIGIN: &str = "https://webmail.example.org";
const BASE: &str = "blob_dir = \"/a/blobs\"\nhostname = \"webmail.example.org\"\n[store]\nurl = \"postgres://rusty_app@db.internal:5432/rusty\"\npassword_file = \"/run/secrets/pg\"\n";

fn config() -> StartupConfig {
    StartupConfig::from_toml_str_for(Role::Webmail, &format!("{BASE}[webmail]\nlisten = \"127.0.0.1:0\"\npublic_origin = \"{ORIGIN}\"\n")).unwrap()
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| l.split_once(':').filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_owned()))
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

async fn call(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&str>) -> Reply {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n\r\n{b}", b.len()));
    } else {
        req.push_str("\r\n");
    }
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let (head, body) = text.split_once("\r\n\r\n").map(|(h, b)| (h.to_owned(), b.to_owned())).unwrap_or_default();
    Reply { status, head, body }
}

const JSON: (&str, &str) = ("Content-Type", "application/json");

async fn sign_in(addr: SocketAddr, address: &str, password: &str) -> Reply {
    let body = serde_json::json!({ "address": address, "password": password }).to_string();
    call(addr, "POST", "/api/v1/session", &[JSON, ("Origin", ORIGIN)], Some(&body)).await
}

fn cookie_of(r: &Reply) -> String {
    r.header("set-cookie").expect("a cookie").split(';').next().unwrap().to_owned()
}

async fn running() -> (Store, pg::PgTestDb, rusty_webmail::run::Running, CancellationToken, Directory) {
    let url = pg::pg_admin_url().expect("RUSTY_TEST_PG_URL");
    // The service runs as its own database role (deploy/db/roles.sql, grants_pg.sql): a query that needs more than that role has fails here. The fixtures (accounts) are made as the operator's role.
    let db = pg::PgTestDb::create_with_roles(&url).await;
    let store = Store::open(StoreConfig::postgres(db.role_url("rusty_webmail"))).await.unwrap();
    let operator = Store::open(StoreConfig::postgres(db.role_url("rusty_app"))).await.unwrap();
    let dir = Directory::with_repo(operator.repos().directory.clone());
    let stop = CancellationToken::new();
    let r = start(&config(), &store, stop.clone()).await.unwrap();
    (store, db, r, stop, dir)
}

#[tokio::test]
async fn a_person_signs_in_sees_who_they_are_and_signs_out_and_every_failure_is_the_same_401() {
    let (_store, _db, r, stop, dir) = running().await;
    let d = dir.create_domain("one.test").await.unwrap();
    dir.create_account("alice", d, Some("correct-horse-battery-1"), None).await.unwrap();

    let ok = sign_in(r.addr, "Alice@One.test", "correct-horse-battery-1").await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert_eq!(ok.json()["address"], serde_json::json!("alice@one.test"), "the address is normalised");
    let csrf = ok.json()["csrf"].as_str().unwrap().to_owned();
    assert_eq!(csrf.len(), 64);
    let set = ok.header("set-cookie").unwrap();
    assert!(set.starts_with("__Host-rusty_webmail=") && set.contains("Secure") && set.contains("HttpOnly") && set.contains("SameSite=Strict") && set.contains("Path=/") && !set.contains("Domain"), "{set}");
    assert_eq!(ok.header("cache-control").as_deref(), Some("no-store"));
    let cookie = cookie_of(&ok);

    // Wrong password, unknown address, malformed address: the same answer, whatever the reason.
    let wrong = sign_in(r.addr, "alice@one.test", "not-the-password").await;
    let unknown = sign_in(r.addr, "nobody@one.test", "correct-horse-battery-1").await;
    let odd = sign_in(r.addr, "not an address", "x").await;
    for f in [&wrong, &unknown, &odd] {
        assert_eq!((f.status, f.body.as_str()), (401, "{\"error\":\"unauthorized\"}"));
        assert!(f.header("set-cookie").is_none());
    }

    // `GET /me` needs the cookie.
    assert_eq!(call(r.addr, "GET", "/api/v1/me", &[], None).await.status, 401);
    assert_eq!(call(r.addr, "GET", "/api/v1/me", &[("Cookie", "__Host-rusty_webmail=00")], None).await.status, 401, "a malformed id");
    let me = call(r.addr, "GET", "/api/v1/me", &[("Cookie", &cookie)], None).await;
    assert_eq!((me.status, me.json()["address"].clone(), me.json()["csrf"].clone()), (200, serde_json::json!("alice@one.test"), serde_json::json!(csrf)));

    // Signing out needs the Origin, the JSON type and the token; then the session is gone.
    let json = "application/json";
    assert_eq!(sign_out(r.addr, &cookie, json, Some(ORIGIN), None).await.status, 403, "no token");
    assert_eq!(sign_out(r.addr, &cookie, json, Some("https://evil.example"), Some(&csrf)).await.status, 403, "another origin");
    assert_eq!(sign_out(r.addr, &cookie, "text/plain", Some(ORIGIN), Some(&csrf)).await.status, 415, "not JSON");
    let out = sign_out(r.addr, &cookie, json, Some(ORIGIN), Some(&csrf)).await;
    assert_eq!(out.status, 200, "{}", out.body);
    assert!(out.header("set-cookie").unwrap().contains("Max-Age=0"), "the cookie is cleared");
    assert_eq!(call(r.addr, "GET", "/api/v1/me", &[("Cookie", &cookie)], None).await.status, 401);

    stop.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(10), r.task).await.is_ok(), "the listener drains");
}

#[tokio::test]
async fn the_sign_in_checks_the_origin_and_the_body_before_anything_else() {
    let (_store, _db, r, stop, dir) = running().await;
    let d = dir.create_domain("one.test").await.unwrap();
    dir.create_account("alice", d, Some("correct-horse-battery-1"), None).await.unwrap();
    let good = serde_json::json!({ "address": "alice@one.test", "password": "correct-horse-battery-1" }).to_string();
    let post = |headers: Vec<(&'static str, &'static str)>, body: String| {
        let addr = r.addr;
        async move { call(addr, "POST", "/api/v1/session", &headers, Some(&body)).await }
    };
    assert_eq!(post(vec![JSON], good.clone()).await.status, 403, "no Origin");
    assert_eq!(post(vec![JSON, ("Origin", "https://webmail.example.org:8443")], good.clone()).await.status, 403, "the port counts");
    assert_eq!(post(vec![("Content-Type", "application/x-www-form-urlencoded"), ("Origin", ORIGIN)], good.clone()).await.status, 415);
    assert_eq!(post(vec![JSON, ("Origin", ORIGIN)], "{\"address\":\"alice@one.test\"}".into()).await.status, 400, "a field is missing");
    assert_eq!(post(vec![JSON, ("Origin", ORIGIN)], "{\"address\":\"a\",\"password\":\"b\",\"extra\":1}".into()).await.status, 400, "an unknown field");
    assert_eq!(post(vec![JSON, ("Origin", ORIGIN)], format!("{{\"address\":\"{}\",\"password\":\"x\"}}", "a".repeat(400))).await.status, 400, "too long");
    assert_eq!(post(vec![JSON, ("Origin", ORIGIN)], good).await.status, 200);
    stop.cancel();
}

#[tokio::test]
async fn repeated_failures_are_throttled_and_a_disabled_account_is_signed_out_at_its_next_request() {
    let (_store, _db, r, stop, dir) = running().await;
    let d = dir.create_domain("one.test").await.unwrap();
    let alice = dir.create_account("alice", d, Some("correct-horse-battery-1"), None).await.unwrap();
    dir.create_account("bob", d, Some("another-long-password-2"), None).await.unwrap();

    // A disabled account: the right password is the same 401, and a live session ends at once.
    let ok = sign_in(r.addr, "alice@one.test", "correct-horse-battery-1").await;
    let cookie = cookie_of(&ok);
    assert_eq!(call(r.addr, "GET", "/api/v1/me", &[("Cookie", &cookie)], None).await.status, 200);
    dir.set_status(alice, AccountStatus::Disabled).await.unwrap();
    assert_eq!(call(r.addr, "GET", "/api/v1/me", &[("Cookie", &cookie)], None).await.status, 401, "disabled meanwhile");
    assert_eq!(sign_in(r.addr, "alice@one.test", "correct-horse-battery-1").await.status, 401, "a disabled account does not sign in");

    // Failures from one address end in a delay, which says how long.
    let mut throttled = None;
    for _ in 0..40 {
        let f = sign_in(r.addr, "bob@one.test", "wrong").await;
        if f.status == 429 {
            throttled = Some(f);
            break;
        }
        assert_eq!(f.status, 401);
    }
    let t = throttled.expect("throttled in the end");
    assert!(t.header("retry-after").and_then(|v| v.parse::<u64>().ok()).is_some_and(|s| s >= 1));
    stop.cancel();
}

#[tokio::test]
async fn the_service_answers_its_own_health_check_and_refuses_to_start_without_its_section() {
    let (_store, _db, r, stop, _dir) = running().await;
    assert!(rusty_webmail::health::healthy(r.addr, Duration::from_secs(5), false).await);
    stop.cancel();

    let url = pg::pg_admin_url().expect("RUSTY_TEST_PG_URL");
    let db = pg::PgTestDb::create_with_roles(&url).await;
    let store = Store::open(StoreConfig::postgres(db.role_url("rusty_webmail"))).await.unwrap();
    let none = StartupConfig::from_toml_str_for(Role::Webmail, BASE).unwrap();
    assert!(start(&none, &store, CancellationToken::new()).await.is_err(), "no [webmail] section");
}

async fn sign_out(addr: SocketAddr, cookie: &str, content_type: &str, origin: Option<&str>, csrf: Option<&str>) -> Reply {
    let mut h = vec![("Cookie", cookie), ("Content-Type", content_type)];
    if let Some(o) = origin {
        h.push(("Origin", o));
    }
    if let Some(c) = csrf {
        h.push(("X-CSRF-Token", c));
    }
    call(addr, "DELETE", "/api/v1/session", &h, Some("{}")).await
}
