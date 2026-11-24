//! The DAV service over a real database and a real socket: Basic sign-in with the own and an app password, the throttle, the challenge, `OPTIONS` without a password, the well-known redirects, one person's
//! tree and no one else's, and the health check. (Run as the service's own database role by `scripts/pg-role-suites.sh`.)

use std::net::SocketAddr;
use std::time::Duration;

use base64::prelude::{Engine, BASE64_STANDARD};
use rusty_core::config::{DavConfig, StartupConfig};
use rusty_directory::Directory;
use rusty_store::fixtures::pg_db as pg;
use rusty_store::{Store, StoreConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

struct World {
    addr: SocketAddr,
    stop: CancellationToken,
    _tmp: tempfile::TempDir,
    _db: pg::PgTestDb,
}

async fn world() -> World {
    let url = pg::pg_admin_url().expect("a Postgres test server (RUSTY_TEST_PG_URL)");
    let db = pg::PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    let seed = Store::open(StoreConfig::postgres(db.seed_url())).await.unwrap();
    let dir = Directory::with_repo(seed.repos().directory.clone());
    let dom = dir.create_domain("one.test").await.unwrap();
    for (local, pw) in [("alice", "alice-password-1"), ("bob", "bob-password-22")] {
        let acc = dir.create_account(local, dom, None, None).await.unwrap();
        dir.set_password(acc, pw).await.unwrap();
        if local == "alice" {
            let hash = rusty_directory::hash_app_password("abcd-efgh-ijkl-mnop").await.unwrap();
            seed.repos().app_passwords.create(acc, "phone", &hash).await.unwrap();
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = StartupConfig::dev(tmp.path());
    cfg.dav = Some(DavConfig { listen: "127.0.0.1:0".parse().unwrap(), ..DavConfig::default() });
    let stop = CancellationToken::new();
    let running = rusty_davd::run::start(&cfg, &store, stop.clone()).await.unwrap();
    World { addr: running.addr, stop, _tmp: tmp, _db: db }
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().find_map(|l| l.split_once(':').filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim()))
    }
}

fn basic(user: &str, pw: &str) -> String {
    format!("Basic {}", BASE64_STANDARD.encode(format!("{user}:{pw}")))
}

async fn call(addr: SocketAddr, method: &str, path: &str, auth: Option<&str>, extra: &[(&str, &str)], body: &str) -> Reply {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: mail.one.test\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: {a}\r\n"));
    }
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    let mut s = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr)).await.unwrap().unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), s.read_to_end(&mut raw)).await.unwrap().unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Reply { status, head: head.to_owned(), body: body.to_owned() }
}

const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//\r\nBEGIN:VEVENT\r\nUID:e1\r\nSUMMARY:Lunch\r\nDTSTART:20240101T100000Z\r\nDTEND:20240101T110000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

#[tokio::test]
async fn a_client_signs_in_with_basic_and_reaches_its_own_tree_only() {
    let w = world().await;
    let alice = basic("alice@one.test", "alice-password-1");
    // OPTIONS needs no password; everything else asks for one, and says how.
    let o = call(w.addr, "OPTIONS", "/dav/", None, &[], "").await;
    assert_eq!(o.status, 200);
    assert!(o.header("dav").unwrap().contains("calendar-access"), "{}", o.head);
    let r = call(w.addr, "PROPFIND", "/dav/", None, &[("Depth", "0")], "").await;
    assert_eq!(r.status, 401);
    assert!(r.header("www-authenticate").unwrap().starts_with("Basic realm="), "{}", r.head);
    assert!(r.header("content-security-policy").is_some(), "the security headers of the HTTP layer are there");
    // With the password: the root names the principal; the collection path has its trailing slash.
    let r = call(w.addr, "PROPFIND", "/dav/", Some(&alice), &[("Depth", "0")], "<D:propfind xmlns:D=\"DAV:\"><D:prop><D:current-user-principal/></D:prop></D:propfind>").await;
    assert_eq!(r.status, 207, "{}", r.body);
    assert!(r.body.contains("/dav/principals/alice%40one.test/"), "{}", r.body);
    let r = call(w.addr, "PROPFIND", "/dav/calendars/alice%40one.test/", Some(&alice), &[("Depth", "1")], "").await;
    assert!(r.status == 207 && r.body.contains("/dav/calendars/alice%40one.test/default/"), "{} {}", r.status, r.body);
    // An event is put and read back through the socket, with the tag.
    let cal = "/dav/calendars/alice%40one.test/default/e1.ics";
    let put = call(w.addr, "PUT", cal, Some(&alice), &[("Content-Type", "text/calendar"), ("If-None-Match", "*")], ICS).await;
    assert_eq!(put.status, 201, "{} {}", put.head, put.body);
    let got = call(w.addr, "GET", cal, Some(&alice), &[], "").await;
    assert_eq!((got.status, got.body.as_str()), (200, ICS));
    assert_eq!(got.header("etag"), put.header("etag"));
    assert!(got.header("content-type").unwrap().starts_with("text/calendar"));
    // Another person cannot read it; a path that is not one is a 404; a method nobody serves is refused.
    let bob = basic("bob@one.test", "bob-password-22");
    assert_eq!(call(w.addr, "GET", cal, Some(&bob), &[], "").await.status, 403);
    assert_eq!(call(w.addr, "GET", "/dav/calendars/bob%40one.test/default/e1.ics", Some(&bob), &[], "").await.status, 404);
    assert_eq!(call(w.addr, "PROPFIND", "/dav/elsewhere", Some(&alice), &[("Depth", "0")], "").await.status, 404);
    assert!(call(w.addr, "LOCK", cal, Some(&alice), &[], "").await.status >= 400);
    // The well-known addresses send a client to the root.
    for p in ["/.well-known/caldav", "/.well-known/carddav"] {
        let r = call(w.addr, "GET", p, None, &[], "").await;
        assert_eq!((r.status, r.header("location")), (301, Some("/dav/")), "{p}");
    }
    w.stop.cancel();
}

#[tokio::test]
async fn an_app_password_signs_in_and_wrong_passwords_are_refused_and_then_throttled() {
    let w = world().await;
    let app = basic("alice@one.test", "abcd-efgh-ijkl-mnop");
    let r = call(w.addr, "PROPFIND", "/dav/principals/alice%40one.test/", Some(&app), &[("Depth", "0")], "").await;
    assert_eq!(r.status, 207, "{} {}", r.head, r.body);
    // The name may be written in another case.
    let upper = basic("Alice@ONE.test", "alice-password-1");
    assert_eq!(call(w.addr, "PROPFIND", "/dav/principals/alice%40one.test/", Some(&upper), &[("Depth", "0")], "").await.status, 207);
    for bad in [basic("alice@one.test", "wrong-password-1"), basic("nobody@one.test", "alice-password-1"), "Basic !!!".to_owned(), "Bearer x".to_owned()] {
        assert_eq!(call(w.addr, "PROPFIND", "/dav/", Some(&bad), &[("Depth", "0")], "").await.status, 401, "{bad}");
    }
    // Enough failures from one address and the answer is "wait".
    let wrong = basic("alice@one.test", "still-wrong-1");
    let mut limited = false;
    for _ in 0..20 {
        let r = call(w.addr, "PROPFIND", "/dav/", Some(&wrong), &[("Depth", "0")], "").await;
        if r.status == 429 {
            assert!(r.header("retry-after").is_some());
            limited = true;
            break;
        }
    }
    assert!(limited, "the throttle never answered");
    w.stop.cancel();
}

#[tokio::test]
async fn the_health_check_asks_options_and_a_closed_port_is_not_healthy() {
    let w = world().await;
    assert!(rusty_davd::health::healthy(w.addr, Duration::from_secs(5), false).await);
    w.stop.cancel();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!rusty_davd::health::healthy(w.addr, Duration::from_secs(1), false).await);
}
