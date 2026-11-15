//! `rusty_admin::run::start`: what the binary assembles. The token exists only while no platform administrator does, the listener is bound when
//! `start` returns, and a configuration without `[admin]` or with a taken port fails the start.

use std::time::Duration;

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
