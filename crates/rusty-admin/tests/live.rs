//! The event stream (`GET /api/v1/events`, docs/design-live-updates.md): it needs a session, starts with `resync`, carries hints of what the API changed to the administrators who may see
//! them and to no one else, keeps a quiet connection alive, ends with the session, and is limited per session.
//!
//! The rig's hub ticks fast (a keep-alive every 300 ms and `monitor` every 200 ms), so the waits below are short.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// An open stream: what has arrived so far, as text (chunk framing included: the tests look for what the frames say).
struct Events {
    tcp: TcpStream,
    got: String,
}

impl Events {
    async fn open(addr: SocketAddr, cookie: &str) -> Events {
        let mut tcp = TcpStream::connect(addr).await.unwrap();
        let req = format!("GET /api/v1/events HTTP/1.1\r\nHost: t\r\nCookie: {cookie}\r\nAccept: text/event-stream\r\n\r\n");
        tcp.write_all(req.as_bytes()).await.unwrap();
        Events { tcp, got: String::new() }
    }

    /// Read until `needle` has arrived (true) or `within` is over (false).
    async fn wait_for(&mut self, needle: &str, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        let mut buf = [0u8; 2048];
        while !self.got.contains(needle) {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            match tokio::time::timeout(left, self.tcp.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => self.got.push_str(&String::from_utf8_lossy(&buf[..n])),
                _ => return false,
            }
        }
        true
    }

    /// Read until `needle` has arrived `times` times.
    async fn wait_count(&mut self, needle: &str, times: usize, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        let mut buf = [0u8; 2048];
        while self.got.matches(needle).count() < times {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            match tokio::time::timeout(left, self.tcp.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => self.got.push_str(&String::from_utf8_lossy(&buf[..n])),
                _ => return false,
            }
        }
        true
    }

    /// Read for `within` and report whether the stream ended (the server closed it).
    async fn ends_within(&mut self, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        let mut buf = [0u8; 2048];
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, self.tcp.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return true,
                Ok(Ok(n)) => self.got.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(_) => return false,
            }
        }
    }
}

const SHORT: Duration = Duration::from_millis(900);
const LONG: Duration = Duration::from_secs(5);

#[tokio::test]
async fn the_stream_needs_a_session_and_starts_with_resync() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    assert_eq!(send(r.addr, "GET", "/api/v1/events", &[], "").await.status, 401);
    let c = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(c.get("/api/v1/events?x=1").await.status, 400, "no query");
    let mut e = Events::open(r.addr, &c.cookie).await;
    assert!(e.wait_for("event: resync", LONG).await, "{}", e.got);
    let head = e.got.to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("content-type: text/event-stream"), "{head}");
    assert!(head.contains("cache-control: no-store"), "{head}");
    assert!(head.contains("x-accel-buffering: no"), "{head}");
}

#[tokio::test]
async fn what_the_api_changes_reaches_the_open_streams_as_a_hint_with_no_data() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    let mut e = Events::open(r.addr, &c.cookie).await;
    assert!(e.wait_for("event: resync", LONG).await);
    assert_eq!(c.post("/api/v1/domains", json!({ "name": "one.test" })).await.status, 201);
    assert!(e.wait_for("\"domains\"", LONG).await, "{}", e.got);
    assert_eq!(c.post("/api/v1/domains/one.test/accounts", json!({ "local": "alice", "password": "alice-password-1" })).await.status, 201);
    assert!(e.wait_for("\"accounts:one.test\"", LONG).await, "{}", e.got);
    assert_eq!(c.post_empty("/api/v1/accounts/alice@one.test/lock").await.status, 200);
    assert!(e.wait_for("\"accounts:one.test\"", LONG).await);
    assert_eq!(c.post("/api/v1/admins", json!({ "address": "alice@one.test", "domain": "one.test" })).await.status, 201);
    assert!(e.wait_for("\"admins\"", LONG).await, "{}", e.got);
    // A hint is a topic and nothing else: no address, no password, no id.
    for secret in ["alice-password-1", "alice@one.test", "password"] {
        assert!(!e.got.contains(secret), "the stream carried {secret}: {}", e.got);
    }
    // A request that failed announces nothing new: the same domain again is refused, and the stream stays quiet about it.
    let before = e.got.matches("\"domains\"").count();
    assert_eq!(c.post("/api/v1/domains", json!({ "name": "one.test" })).await.status, 409);
    assert!(!e.wait_for("never-sent", SHORT).await);
    assert_eq!(e.got.matches("\"domains\"").count(), before, "a refused request was announced: {}", e.got);
}

#[tokio::test]
async fn a_domain_administrator_hears_about_its_own_domain_and_never_about_another_or_the_monitor() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "boss@one.test", true).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    let boss = Client::sign_in(r.addr, "boss@one.test").await;
    root.post("/api/v1/domains", json!({ "name": "two.test" })).await;
    let mut e = Events::open(r.addr, &boss.cookie).await;
    assert!(e.wait_for("event: resync", LONG).await);
    // Changes in the other tenant's domain: nothing arrives.
    root.post("/api/v1/domains/two.test/accounts", json!({ "local": "zed", "password": "zed-password-12" })).await;
    root.mutate("PUT", "/api/v1/domains/two.test/relay", Some(json!({ "host": "smtp.relay.example", "port": 587, "tls": "starttls" }))).await;
    assert!(!e.wait_for("two.test", SHORT).await, "another tenant's topic reached a domain administrator: {}", e.got);
    // Its own domain's: it arrives.
    root.post("/api/v1/domains/one.test/accounts", json!({ "local": "carl", "password": "carl-password-12" })).await;
    assert!(e.wait_for("\"accounts:one.test\"", LONG).await, "{}", e.got);
    // The monitor is for platform administrators: even with its tick every 200 ms, not a word of it here.
    assert!(!e.got.contains("monitor"), "{}", e.got);
}

#[tokio::test]
async fn a_platform_stream_ticks_the_monitor_and_every_stream_is_kept_alive() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    let mut e = Events::open(r.addr, &c.cookie).await;
    assert!(e.wait_for("\"monitor\"", LONG).await, "{}", e.got);
    assert!(e.wait_for(": keep-alive", LONG).await, "{}", e.got);
}

#[tokio::test]
async fn the_stream_ends_with_the_session_and_with_the_grant() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "boss@home.test", true).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    let boss = Client::sign_in(r.addr, "boss@home.test").await;
    let mut by_root = Events::open(r.addr, &root.cookie).await;
    let mut by_boss = Events::open(r.addr, &boss.cookie).await;
    assert!(by_root.wait_for("event: resync", LONG).await && by_boss.wait_for("event: resync", LONG).await);
    // Signing out ends the stream at the next look at the session (the next keep-alive, 300 ms here).
    assert_eq!(root.mutate("DELETE", "/api/v1/session", None).await.status, 200);
    assert!(by_root.ends_within(LONG).await, "the stream outlived its session");
    // So does losing the grant (another administrator revokes it): the keep-alive finds no administrator.
    let second = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(second.mutate("DELETE", "/api/v1/admins/boss@home.test", None).await.status, 200);
    assert!(by_boss.ends_within(LONG).await, "the stream outlived its grant");
}

#[tokio::test]
async fn a_session_may_hold_a_few_streams_and_no_more() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    let mut held = Vec::new();
    for _ in 0..4 {
        let mut e = Events::open(r.addr, &c.cookie).await;
        assert!(e.wait_for("event: resync", LONG).await, "{}", e.got);
        held.push(e);
    }
    let fifth = c.get("/api/v1/events").await;
    assert_eq!(fifth.status, 429, "{}", fifth.head);
    assert_eq!(fifth.json()["error"], "throttled");
    // Closing one makes room.
    held.pop();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let mut again = Events::open(r.addr, &c.cookie).await;
    assert!(again.wait_for("event: resync", LONG).await, "{}", again.got);
}

#[tokio::test]
async fn what_another_process_writes_reaches_the_streams_through_the_database_for_the_right_domain_only() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "boss@one.test", true).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    let boss = Client::sign_in(r.addr, "boss@one.test").await;
    root.post("/api/v1/domains", json!({ "name": "two.test" })).await;
    // The admin service listens to the database, as run.rs makes it do.
    rusty_admin::live::spawn_feed(r.app.clone(), r.store.admin_feed(), r.stop.clone());
    let mut by_root = Events::open(r.addr, &root.cookie).await;
    let mut by_boss = Events::open(r.addr, &boss.cookie).await;
    assert!(by_root.wait_for("event: resync", LONG).await && by_boss.wait_for("event: resync", LONG).await);

    // Another process (here: raw SQL, as the outbound service's recording of a relay check would be) changes the relay of two.test: the platform administrator hears of it, the administrator of
    // one.test does not. The database feed may still be subscribing (its connection is one of many when the whole suite runs), and a change made before that is not announced: so the change is
    // made again until it is heard, which is also what a person's next attempt would be.
    let two = r.dir.find_domain("two.test").await.unwrap().unwrap().id.0;
    r.fx.run(&format!("INSERT INTO domain_relays (domain_id, host, port, tls, created_at, updated_at) VALUES ({two}, 'smtp.relay.example', 587, 'starttls', 1, 1)"), &[]).await;
    let mut heard = false;
    for i in 0..90 {
        if by_root.wait_for("\"outbound:two.test\"", Duration::from_secs(1)).await {
            heard = true;
            break;
        }
        r.fx.run(&format!("UPDATE domain_relays SET port = {} WHERE domain_id = {two}", if i % 2 == 0 { 2525 } else { 587 }), &[]).await;
    }
    assert!(heard, "{}", by_root.got);
    assert!(!by_boss.wait_for("two.test", SHORT).await, "{}", by_boss.got);
    // And a change of one.test's own: the domain administrator hears of it.
    let one = r.dir.find_domain("one.test").await.unwrap().unwrap().id.0;
    r.fx.run(&format!("INSERT INTO domain_relays (domain_id, host, port, tls, created_at, updated_at) VALUES ({one}, 'smtp.relay.example', 587, 'starttls', 1, 1)"), &[]).await;
    assert!(by_boss.wait_for("\"outbound:one.test\"", LONG).await, "{}", by_boss.got);
    assert!(!by_boss.got.contains("two.test"), "{}", by_boss.got);
}
