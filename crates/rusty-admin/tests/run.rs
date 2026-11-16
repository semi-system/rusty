//! `rusty_admin::run::start`: what the binary assembles. The token exists only while no platform administrator does, the listener is bound when
//! `start` returns, and a configuration without `[admin]` or with a taken port fails the start.

use std::time::Duration;

mod common;

use rusty_admin::run::start;
use rusty_core::config::{Role, StartupConfig};
use rusty_directory::Directory;
use rusty_store::fixtures::pg_db as pg;
use rusty_store::{AdminStore, Store, StoreConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

const BASE: &str = "hostname = \"admin.example.org\"\n[store]\nurl = \"postgres://rusty_app@db.internal:5432/rusty\"\npassword_file = \"/run/secrets/pg\"\n";

fn config(admin: &str) -> StartupConfig {
    StartupConfig::from_toml_str_for(Role::Admin, &format!("{BASE}{admin}")).unwrap()
}

async fn store() -> (Store, pg::PgTestDb) {
    let url = pg::pg_admin_url().expect("RUSTY_TEST_PG_URL");
    let db = pg::PgTestDb::create(&url).await;
    (Store::open(StoreConfig::postgres(db.url())).await.unwrap(), db)
}

async fn get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, text.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default())
}

#[tokio::test]
async fn with_no_administrator_a_token_is_made_and_the_setup_page_is_open() {
    let (store, _db) = store().await;
    let cfg = config("[admin]\nlisten = \"127.0.0.1:0\"\npublic_origin = \"https://admin.example.org\"\n");
    let stop = CancellationToken::new();
    let r = start(&cfg, &store, stop.clone()).await.unwrap();
    let token = r.setup_token.clone().expect("a token");
    assert_eq!(token.len(), 64);
    assert_eq!(get(r.addr, "/api/v1/setup").await, (200, "{\"required\":true}".into()));
    assert_eq!(get(r.addr, "/api/v1/session").await.0, 401);
    stop.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(10), r.task).await.is_ok(), "the listener drains");
}

#[tokio::test]
async fn with_an_administrator_there_is_no_token_and_no_setup_page() {
    let (store, _db) = store().await;
    let dir = Directory::with_repo(store.repos().directory.clone());
    let d = dir.create_domain("one.test").await.unwrap();
    let a = dir.create_account("root", d, Some("correct-horse-battery-1"), None).await.unwrap();
    AdminStore::with_repo(store.repos().admin.clone()).grant(a, None).await.unwrap();
    let cfg = config("[admin]\nlisten = \"127.0.0.1:0\"\npublic_origin = \"https://admin.example.org\"\n");
    let stop = CancellationToken::new();
    let r = start(&cfg, &store, stop.clone()).await.unwrap();
    assert!(r.setup_token.is_none());
    assert_eq!(get(r.addr, "/api/v1/setup").await.0, 404);
    stop.cancel();
}

#[tokio::test]
async fn a_new_token_is_made_at_every_start() {
    let (store, _db) = store().await;
    let cfg = config("[admin]\nlisten = \"127.0.0.1:0\"\npublic_origin = \"https://admin.example.org\"\n");
    let (s1, s2) = (CancellationToken::new(), CancellationToken::new());
    let a = start(&cfg, &store, s1.clone()).await.unwrap().setup_token.unwrap();
    let b = start(&cfg, &store, s2.clone()).await.unwrap().setup_token.unwrap();
    assert_ne!(a, b);
    s1.cancel();
    s2.cancel();
}

#[tokio::test]
async fn a_missing_admin_section_or_a_taken_port_fails_the_start() {
    let (store, _db) = store().await;
    let no_admin = StartupConfig::from_toml_str_for(Role::Admin, BASE).unwrap();
    assert!(start(&no_admin, &store, CancellationToken::new()).await.is_err());
    let cfg = config("[admin]\nlisten = \"127.0.0.1:0\"\npublic_origin = \"https://admin.example.org\"\n");
    let first = start(&cfg, &store, CancellationToken::new()).await.unwrap();
    let taken = config(&format!("[admin]\nlisten = \"{}\"\npublic_origin = \"https://admin.example.org\"\n", first.addr));
    assert!(start(&taken, &store, CancellationToken::new()).await.is_err(), "the port is in use");
}

fn valkey_url() -> Option<String> {
    match std::env::var("RUSTY_TEST_VALKEY_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            assert!(std::env::var("RUSTY_REQUIRE_VALKEY").as_deref() != Ok("1"), "RUSTY_REQUIRE_VALKEY=1 but RUSTY_TEST_VALKEY_URL is not set");
            None
        }
    }
}

/// `[valkey]` text for the test server, the password in a 0600 file inside `dir`.
fn valkey_section(url: &str, dir: &std::path::Path, prefix: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let rest = url.strip_prefix("valkey://").unwrap();
    let (cred, hostport) = rest.split_once('@').unwrap();
    let (user, password) = cred.split_once(':').unwrap();
    let (host, port) = hostport.rsplit_once(':').unwrap();
    let pw = dir.join("valkey.pw");
    std::fs::write(&pw, password).unwrap();
    std::fs::set_permissions(&pw, std::fs::Permissions::from_mode(0o600)).unwrap();
    format!("[valkey]\nhost = \"{host}\"\nport = {port}\nusername = \"{user}\"\npassword_file = \"{}\"\nkey_prefix = \"{prefix}\"\n", pw.display())
}

const ADMIN: &str = "[admin]\nlisten = \"127.0.0.1:0\"\npublic_origin = \"https://admin.example.org\"\n";

#[tokio::test]
async fn a_configured_valkey_that_cannot_be_reached_fails_the_start_instead_of_falling_back_to_memory() {
    let (store, _db) = store().await;
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let cfg = config(&format!("{ADMIN}[valkey]\nhost = \"127.0.0.1\"\nport = {port}\nconnect_timeout_ms = 500\n"));
    assert!(start(&cfg, &store, CancellationToken::new()).await.is_err());
}

#[tokio::test]
async fn sessions_kept_in_valkey_are_shared_by_two_instances_and_survive_a_restart() {
    let Some(url) = valkey_url() else { return };
    let (store, _db) = store().await;
    let dir = Directory::with_repo(store.repos().directory.clone());
    let d = dir.create_domain("one.test").await.unwrap();
    let a = dir.create_account("root", d, Some(common::PW), None).await.unwrap();
    AdminStore::with_repo(store.repos().admin.clone()).grant(a, None).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(&format!("{ADMIN}{}", valkey_section(&url, tmp.path(), &format!("rt{}", std::process::id()))));

    let stop1 = CancellationToken::new();
    let first = start(&cfg, &store, stop1.clone()).await.unwrap();
    let client = common::Client::sign_in(first.addr, "root@one.test").await;
    assert_eq!(client.get("/api/v1/session").await.status, 200);

    // A second instance (another process in production) and, after the first stops, a restart: the same cookie is still a session.
    let stop2 = CancellationToken::new();
    let second = start(&cfg, &store, stop2.clone()).await.unwrap();
    let on_second = common::Client { addr: second.addr, cookie: client.cookie.clone(), csrf: client.csrf.clone() };
    assert_eq!(on_second.get("/api/v1/session").await.status, 200, "the other instance knows the session");
    stop1.cancel();
    let _ = first.task.await;
    assert_eq!(on_second.get("/api/v1/session").await.status, 200, "and it outlives the instance that made it");
    stop2.cancel();
}

#[tokio::test]
async fn without_valkey_a_session_belongs_to_one_process() {
    let (store, _db) = store().await;
    let dir = Directory::with_repo(store.repos().directory.clone());
    let d = dir.create_domain("one.test").await.unwrap();
    let a = dir.create_account("root", d, Some(common::PW), None).await.unwrap();
    AdminStore::with_repo(store.repos().admin.clone()).grant(a, None).await.unwrap();
    let cfg = config(ADMIN);
    let first = start(&cfg, &store, CancellationToken::new()).await.unwrap();
    let client = common::Client::sign_in(first.addr, "root@one.test").await;
    let second = start(&cfg, &store, CancellationToken::new()).await.unwrap();
    let elsewhere = common::Client { addr: second.addr, cookie: client.cookie.clone(), csrf: client.csrf.clone() };
    assert_eq!(elsewhere.get("/api/v1/session").await.status, 401, "the memory store is not shared");
}

#[tokio::test]
async fn with_a_metrics_section_the_admin_web_serves_the_platform_figures_to_the_allowed_scraper() {
    let (store, _db) = store().await;
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = config(&format!(
        "[admin]\nlisten = \"127.0.0.1:0\"\npublic_origin = \"https://admin.example.org\"\n[metrics]\nlisten = \"127.0.0.1:{port}\"\nallowed_peers = [\"127.0.0.1/32\"]\n"
    ));
    let stop = CancellationToken::new();
    let r = start(&cfg, &store, stop.clone()).await.unwrap();
    // The admin web beats at once; the gauges that read the heartbeats show it as up.
    let mut body = String::new();
    for _ in 0..40 {
        body = get(format!("127.0.0.1:{port}").parse().unwrap(), "/metrics").await.1;
        if body.contains("rusty_service_up{service=\"admin\"} 1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(body.contains("rusty_build_info{service=\"admin\","), "{body}");
    assert!(body.contains("rusty_service_up{service=\"admin\"} 1"), "{body}");
    assert!(body.contains("rusty_service_up{service=\"imap\"} 0"), "a service that never beat is down: {body}");
    assert!(body.contains("rusty_outbound_queue_jobs{state=\"queued\"} 0"), "{body}");
    assert!(body.contains("rusty_dns_problem_domains 0"), "{body}");
    // The metrics port is not the admin port: the admin listener does not serve it.
    assert!(!get(r.addr, "/metrics").await.1.contains("rusty_build_info"), "the admin listener does not serve the metrics");
    stop.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(10), r.task).await.is_ok());
}

#[tokio::test]
async fn with_scrape_targets_the_admin_web_is_the_one_endpoint_for_every_service() {
    let (store, _db) = store().await;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dead = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cfg = config(&format!(
        "[admin]\nlisten = \"127.0.0.1:0\"\npublic_origin = \"https://admin.example.org\"\n[metrics]\nlisten = \"127.0.0.1:{port}\"\nallowed_peers = [\"127.0.0.1/32\"]\n\
         [[metrics.scrape]]\nservice = \"self\"\naddress = \"127.0.0.1:{port}\"\n[[metrics.scrape]]\nservice = \"imap\"\naddress = \"127.0.0.1:{dead}\"\n"
    ));
    let stop = CancellationToken::new();
    let r = start(&cfg, &store, stop.clone()).await.unwrap();
    let body = get(format!("127.0.0.1:{port}").parse().unwrap(), "/metrics")
        .await
        .1;
    assert!(
        body.contains(&format!(
            "rusty_scrape_target_up{{instance=\"127.0.0.1:{port}\",service=\"self\"}} 1"
        )),
        "the endpoint reads a service it can reach: {body}"
    );
    assert!(
        body.contains(&format!(
            "rusty_scrape_target_up{{instance=\"127.0.0.1:{dead}\",service=\"imap\"}} 0"
        )),
        "a service it cannot reach is down, not missing: {body}"
    );
    assert!(
        body.contains("rusty_build_info{instance=\"local\",service=\"admin\",version="),
        "its own samples are labelled: {body}"
    );
    stop.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(10), r.task)
        .await
        .is_ok());
}
