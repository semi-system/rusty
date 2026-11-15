//! The session and setup API over a real listener and a real directory: cookies, CSRF guards, the fixed errors, the setup token and its
//! throttle, and "the setup page is gone the moment an administrator exists".

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rusty_admin::api::routes;
use rusty_admin::{AdminApp, AdminAuth, ManualNow, SessionConfig, SessionStore, SetupGate, SystemNow};
use rusty_directory::{Directory, LocalAuthenticator};
use rusty_http::{HttpConfig, HttpHandler, Router};
use rusty_store::fixtures::pg_db as pg;
use rusty_store::{AdminStore, Store, StoreConfig};
use rusty_svc::{AuthThrottle, BoundListener, DirectTcp, ListenerSpec, SystemClock, TracingEvents};
use rusty_valkey::MemoryKv;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

const ORIGIN: &str = "https://admin.example.org";
const PW: &str = "correct-horse-battery-1";

struct Rig {
    addr: SocketAddr,
    dir: Directory,
    admins: AdminStore,
    token: Option<String>,
    stop: CancellationToken,
    _store: Store,
    _db: pg::PgTestDb,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn rig(with_setup: bool, max_failures: u32) -> Rig {
    let url = pg::pg_admin_url().expect("RUSTY_TEST_PG_URL");
    let db = pg::PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    let dir = Directory::with_repo(store.repos().directory.clone());
    let admins = AdminStore::with_repo(store.repos().admin.clone());
    let sessions = SessionStore::new(Arc::new(MemoryKv::new()), SessionConfig::default()).with_clock(Arc::new(ManualNow::new(1_000_000)));
    let throttle = Arc::new(AuthThrottle::new(max_failures, 600, 4, Arc::new(SystemClock::new())));
    let authenticator = Arc::new(LocalAuthenticator::new(dir.clone()).await.unwrap());
    let auth = AdminAuth::new(authenticator, dir.clone(), admins.clone(), sessions, throttle);
    let (gate, token) = if with_setup {
        let (g, t) = SetupGate::new(Arc::new(SystemNow));
        (Some(g), Some(t))
    } else {
        (None, None)
    };
    let app = Arc::new(AdminApp::new(auth, dir.clone(), admins.clone(), gate, ORIGIN, SessionConfig::default()));
    let h = HttpHandler::new(routes(app, Router::new()), HttpConfig::default());
    let spec = ListenerSpec { name: "admin-test", addr: "127.0.0.1:0".parse().unwrap(), max_connections: 64, max_per_ip: 64 };
    let bound = BoundListener::bind(spec).await.unwrap();
    let addr = bound.local_addr();
    let stop = CancellationToken::new();
    let s = stop.clone();
    tokio::spawn(async move {
        let _ = bound.serve(Arc::new(DirectTcp), Arc::new(h), Arc::new(TracingEvents), s, Duration::from_secs(5)).await;
    });
    Rig { addr, dir, admins, token, stop, _store: store, _db: db }
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        // Names match case-insensitively; the value keeps its case (a cookie name is case-sensitive).
        self.head.split("\r\n").skip(1).find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
    /// The `name=value` part of the Set-Cookie header.
    fn cookie(&self) -> Option<String> {
        self.header("set-cookie")?.split(';').next().map(str::to_owned)
    }
}

async fn send(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: admin.example.org\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (n, v) in headers {
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(8), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    Reply { status, head: format!("{head}\r\n"), body: body.to_owned() }
}

const JSON: (&str, &str) = ("Content-Type", "application/json");
const ORIG: (&str, &str) = ("Origin", ORIGIN);

async fn post(addr: SocketAddr, path: &str, body: &str) -> Reply {
    send(addr, "POST", path, &[JSON, ORIG], body).await
}

fn creds(address: &str, password: &str) -> String {
    serde_json::json!({ "address": address, "password": password }).to_string()
}

fn setup_body(token: &str, address: &str, password: &str) -> String {
    serde_json::json!({ "token": token, "address": address, "password": password }).to_string()
}

async fn make_admin(r: &Rig, address: &str, domain_scoped: bool) {
    let (local, domain) = address.split_once('@').unwrap();
    let d = match r.dir.find_domain(domain).await.unwrap() {
        Some(d) => d.id,
        None => r.dir.create_domain(domain).await.unwrap(),
    };
    let a = r.dir.create_account(local, d, Some(PW), None).await.unwrap();
    r.admins.grant(a, domain_scoped.then_some(d)).await.unwrap();
}

#[tokio::test]
async fn signing_in_sets_a_host_cookie_and_whoami_and_sign_out_follow_the_csrf_rules() {
    let r = rig(false, 50).await;
    make_admin(&r, "root@one.test", false).await;
    let a = r.addr;
    // Origin and content type are checked before anything else.
    let bad_origin = send(a, "POST", "/api/v1/session", &[JSON, ("Origin", "https://evil.example")], &creds("root@one.test", PW)).await;
    assert_eq!((bad_origin.status, bad_origin.json()["error"].clone()), (403, "forbidden".into()));
    let no_origin = send(a, "POST", "/api/v1/session", &[JSON], &creds("root@one.test", PW)).await;
    assert_eq!(no_origin.status, 403);
    let form = send(a, "POST", "/api/v1/session", &[("Content-Type", "application/x-www-form-urlencoded"), ORIG], "address=a&password=b").await;
    assert_eq!(form.status, 415);
    // Unknown fields and a non-JSON body are `invalid`.
    assert_eq!(post(a, "/api/v1/session", "{\"address\":\"a@b.test\",\"password\":\"x\",\"extra\":1}").await.status, 400);
    assert_eq!(post(a, "/api/v1/session", "not json").await.status, 400);
    // The right credentials.
    let ok = post(a, "/api/v1/session", &creds("Root@One.Test", PW)).await;
    assert_eq!(ok.status, 200, "{}", ok.head);
    let cookie = ok.cookie().unwrap();
    assert!(cookie.starts_with("__Host-rusty_admin="), "{cookie}");
    let set = ok.header("set-cookie").unwrap().to_ascii_lowercase();
    for attr in ["secure", "httponly", "samesite=strict", "path=/"] {
        assert!(set.contains(attr), "{set}");
    }
    assert!(!set.contains("domain="), "{set}");
    let me = ok.json();
    assert_eq!((me["address"].as_str(), me["scope"].as_str(), me["domain"].is_null()), (Some("root@one.test"), Some("platform"), true));
    let csrf = me["csrf"].as_str().unwrap().to_owned();
    assert_eq!(csrf.len(), 64);
    assert_eq!(ok.header("cache-control").as_deref(), Some("no-store"));
    // Who am I: with the cookie yes (and the same CSRF token), without it no.
    let who = send(a, "GET", "/api/v1/session", &[("Cookie", &cookie)], "").await;
    assert_eq!((who.status, who.json()["csrf"].as_str()), (200, Some(csrf.as_str())));
    assert_eq!(send(a, "GET", "/api/v1/session", &[], "").await.status, 401);
    // Sign out needs all three guards.
    let zero = "0".repeat(64);
    let h = |origin: &'static str, ctype: &'static str, token: Option<&str>| {
        let mut v = vec![("Cookie", cookie.clone()), ("Origin", origin.to_owned()), ("Content-Type", ctype.to_owned())];
        if let Some(t) = token {
            v.push(("X-CSRF-Token", t.to_owned()));
        }
        v
    };
    async fn del(a: SocketAddr, headers: Vec<(&str, String)>) -> Reply {
        let refs: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
        send(a, "DELETE", "/api/v1/session", &refs, "").await
    }
    assert_eq!(del(a, h(ORIGIN, "application/json", None)).await.status, 403, "no CSRF token");
    assert_eq!(del(a, h(ORIGIN, "application/json", Some(&zero))).await.status, 403, "wrong CSRF token");
    assert_eq!(del(a, h("https://evil.example", "application/json", Some(&csrf))).await.status, 403, "wrong origin");
    assert_eq!(del(a, h(ORIGIN, "text/plain", Some(&csrf))).await.status, 415, "wrong content type");
    assert_eq!(send(a, "GET", "/api/v1/session", &[("Cookie", &cookie)], "").await.status, 200, "the refused attempts did not end it");
    let out = del(a, h(ORIGIN, "application/json", Some(&csrf))).await;
    assert_eq!(out.status, 200, "{}", out.head);
    assert!(out.header("set-cookie").unwrap().to_ascii_lowercase().contains("max-age=0"));
    assert_eq!(send(a, "GET", "/api/v1/session", &[("Cookie", &cookie)], "").await.status, 401, "the session is gone");
}

#[tokio::test]
async fn every_failed_sign_in_is_the_same_401_and_probing_is_throttled() {
    let r = rig(false, 3).await;
    make_admin(&r, "root@one.test", false).await;
    let plain = r.dir.create_account("plain", r.dir.find_domain("one.test").await.unwrap().unwrap().id, Some(PW), None).await.unwrap();
    let _ = plain;
    let a = r.addr;
    let mut bodies = Vec::new();
    for (who, pw) in [("root@one.test", "wrong-password-1"), ("ghost@one.test", PW), ("plain@one.test", PW)] {
        let x = post(a, "/api/v1/session", &creds(who, pw)).await;
        assert_eq!(x.status, 401, "{who}");
        assert!(x.header("set-cookie").is_none());
        bodies.push(x.body);
    }
    assert!(bodies.iter().all(|b| b == &bodies[0] && b == "{\"error\":\"unauthorized\"}"), "{bodies:?}");
    // The counter is per (address, account): two more wrong passwords for root make three.
    for _ in 0..2 {
        assert_eq!(post(a, "/api/v1/session", &creds("root@one.test", "wrong-password-1")).await.status, 401);
    }
    // Three failures from this address for this account: the next one, even with the right password, is refused with a delay.
    let blocked = post(a, "/api/v1/session", &creds("root@one.test", PW)).await;
    assert_eq!((blocked.status, blocked.json()["error"].clone()), (429, "throttled".into()), "{}", blocked.head);
    assert!(blocked.header("retry-after").unwrap().parse::<u64>().unwrap() >= 1);
}

#[tokio::test]
async fn a_domain_administrator_sees_its_scope_and_losing_the_grant_ends_the_session() {
    let r = rig(false, 50).await;
    make_admin(&r, "boss@two.test", true).await;
    let a = r.addr;
    let ok = post(a, "/api/v1/session", &creds("boss@two.test", PW)).await;
    assert_eq!(ok.json()["scope"], "domain");
    assert!(ok.json()["domain"].is_i64());
    let cookie = ok.cookie().unwrap();
    assert_eq!(send(a, "GET", "/api/v1/session", &[("Cookie", &cookie)], "").await.status, 200);
    let acc = r.dir.find_account("boss@two.test").await.unwrap().unwrap();
    r.admins.revoke(acc.id).await.unwrap();
    assert_eq!(send(a, "GET", "/api/v1/session", &[("Cookie", &cookie)], "").await.status, 401, "the database decides at every request");
}

#[tokio::test]
async fn a_forged_ambiguous_or_malformed_cookie_is_not_a_session() {
    let r = rig(false, 50).await;
    make_admin(&r, "root@one.test", false).await;
    let a = r.addr;
    let ok = post(a, "/api/v1/session", &creds("root@one.test", PW)).await;
    let cookie = ok.cookie().unwrap();
    let value = cookie.split_once('=').unwrap().1.to_owned();
    for bad in [
        "__Host-rusty_admin=".to_owned(),
        "__Host-rusty_admin=junk".to_owned(),
        format!("__Host-rusty_admin={}", "a".repeat(64)),
        // Two cookies of the same name, one valid: ambiguous, so none.
        format!("{cookie}; __Host-rusty_admin={}", "b".repeat(64)),
        // A cookie of another name carrying the value.
        format!("rusty_admin={value}"),
    ] {
        assert_eq!(send(a, "GET", "/api/v1/session", &[("Cookie", &bad)], "").await.status, 401, "{bad}");
    }
}

#[tokio::test]
async fn the_setup_page_makes_the_first_administrator_with_the_token_and_vanishes() {
    let r = rig(true, 50).await;
    let a = r.addr;
    let token = r.token.clone().unwrap();
    assert_eq!(token.len(), 64);
    let state = send(a, "GET", "/api/v1/setup", &[], "").await;
    assert_eq!((state.status, state.json()["required"].clone()), (200, true.into()));
    assert_eq!(state.header("cache-control").as_deref(), Some("no-store"));
    // Guards first: origin and content type.
    assert_eq!(send(a, "POST", "/api/v1/setup", &[JSON], &setup_body(&token, "root@one.test", PW)).await.status, 403);
    assert_eq!(send(a, "POST", "/api/v1/setup", &[("Content-Type", "text/plain"), ORIG], &setup_body(&token, "root@one.test", PW)).await.status, 415);
    // A wrong token is 401 and makes nothing.
    let wrong = post(a, "/api/v1/setup", &setup_body(&"0".repeat(64), "root@one.test", PW)).await;
    assert_eq!((wrong.status, wrong.body.as_str()), (401, "{\"error\":\"unauthorized\"}"));
    assert!(r.dir.find_domain("one.test").await.unwrap().is_none());
    // The right token with a bad address or a short password is 400 and does not spend the token.
    assert_eq!(post(a, "/api/v1/setup", &setup_body(&token, "not an address", PW)).await.status, 400);
    assert_eq!(post(a, "/api/v1/setup", &setup_body(&token, "root@one.test", "short")).await.status, 400);
    assert!(!r.admins.has_platform_admin().await.unwrap());
    // The right token and a good password.
    let done = post(a, "/api/v1/setup", &setup_body(&token.to_uppercase(), "Root@One.Test", PW)).await;
    assert_eq!(done.status, 201, "{} {}", done.head, done.body);
    assert_eq!(done.json()["scope"], "platform");
    let cookie = done.cookie().expect("signed in at once");
    assert_eq!(send(a, "GET", "/api/v1/session", &[("Cookie", &cookie)], "").await.status, 200);
    assert!(r.admins.has_platform_admin().await.unwrap());
    // The page is gone for good, even with the right token.
    assert_eq!(send(a, "GET", "/api/v1/setup", &[], "").await.status, 404);
    assert_eq!(post(a, "/api/v1/setup", &setup_body(&token, "other@one.test", PW)).await.status, 404);
    assert!(r.dir.find_account("other@one.test").await.unwrap().is_none());
}

#[tokio::test]
async fn the_setup_page_does_not_exist_when_no_token_was_made_or_an_administrator_already_exists() {
    // Started with an administrator: no token, no page.
    let r = rig(false, 50).await;
    assert_eq!(send(r.addr, "GET", "/api/v1/setup", &[], "").await.status, 404);
    assert_eq!(post(r.addr, "/api/v1/setup", &setup_body(&"a".repeat(64), "root@one.test", PW)).await.status, 404);
    // A token is outstanding but an administrator was made meanwhile (rustyctl admin grant): a database fact, so the page is gone.
    let r2 = rig(true, 50).await;
    let token = r2.token.clone().unwrap();
    make_admin(&r2, "root@one.test", false).await;
    assert_eq!(send(r2.addr, "GET", "/api/v1/setup", &[], "").await.status, 404);
    assert_eq!(post(r2.addr, "/api/v1/setup", &setup_body(&token, "second@one.test", PW)).await.status, 404);
}

#[tokio::test]
async fn five_wrong_setup_tokens_from_one_address_lock_that_address_even_for_the_right_token() {
    let r = rig(true, 50).await;
    let a = r.addr;
    let token = r.token.clone().unwrap();
    for _ in 0..5 {
        assert_eq!(post(a, "/api/v1/setup", &setup_body(&"1".repeat(64), "root@one.test", PW)).await.status, 401);
    }
    let locked = post(a, "/api/v1/setup", &setup_body(&token, "root@one.test", PW)).await;
    assert_eq!((locked.status, locked.json()["error"].clone()), (429, "throttled".into()), "{}", locked.head);
    assert!(locked.header("retry-after").unwrap().parse::<u64>().unwrap() > 3000);
    assert!(!r.admins.has_platform_admin().await.unwrap(), "the right token did not get through while locked");
}

#[tokio::test]
async fn two_setups_at_once_make_one_administrator() {
    let r = rig(true, 50).await;
    let a = r.addr;
    let token = r.token.clone().unwrap();
    let mut tasks = Vec::new();
    for i in 0..6 {
        let t = token.clone();
        tasks.push(tokio::spawn(async move { post(a, "/api/v1/setup", &setup_body(&t, &format!("admin{i}@one.test"), PW)).await.status }));
    }
    let mut statuses = Vec::new();
    for t in tasks {
        statuses.push(t.await.unwrap());
    }
    assert_eq!(statuses.iter().filter(|s| **s == 201).count(), 1, "{statuses:?}");
    assert!(statuses.iter().all(|s| *s == 201 || *s == 404), "{statuses:?}");
    let mut n = 0;
    let mut after = None;
    loop {
        let page = r.admins.list(after, 50).await.unwrap();
        n += page.len();
        after = page.last().map(|g| g.account);
        if page.len() < 50 {
            break;
        }
    }
    assert_eq!(n, 1, "exactly one grant exists");
}
