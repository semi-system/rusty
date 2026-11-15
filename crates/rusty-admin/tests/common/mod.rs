#![allow(dead_code)]
//! Shared harness of the admin endpoint tests: a real listener over a real directory, a signed-in client and the contract checks.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rusty_admin::api::routes as session_routes;
use rusty_admin::manage::routes as manage_routes;
use rusty_admin::outbound::routes as outbound_routes;
use rusty_admin::storage::routes as storage_routes;
use rusty_admin::{AdminApp, AdminAuth, MailSettings, ManualNow, Services, SessionConfig, SessionStore};
use rusty_directory::{Directory, LocalAuthenticator};
use rusty_http::{HttpConfig, HttpHandler, Router};
use rusty_store::fixtures::pg_db as pg;
use rusty_store::{AdminStore, Store, StoreConfig};
use rusty_svc::{AuthThrottle, BoundListener, DirectTcp, ListenerSpec, SystemClock, TracingEvents};
use rusty_valkey::MemoryKv;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

pub const ORIGIN: &str = "https://admin.example.org";
pub const PW: &str = "correct-horse-battery-1";
pub const CONTRACT: &str = include_str!("../../contract/v1.json");

pub struct Rig {
    pub addr: SocketAddr,
    pub dir: Directory,
    pub admins: AdminStore,
    stop: CancellationToken,
    pub now: Arc<ManualNow>,
    /// Opens what the app sealed (the outbound service's key).
    pub unsealer: rusty_seal::Unsealer,
    pub store: Store,
    /// The outbound service's view of the database (role `rusty_app`): the only one that may read a sealed secret.
    pub outbound: Store,
    pub fx: rusty_store::fixtures::Fixtures,
    _db: pg::PgTestDb,
}

impl Rig {
    /// Jobs of `kind` in the queue, any state.
    pub async fn count_jobs(&self, kind: &str) -> i64 {
        self.fx.scalar(&format!("SELECT count(*) FROM jobs WHERE kind = '{kind}'"), &[]).await
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub async fn rig() -> Rig {
    build(true).await
}

/// A deployment without `[sealing]`: nothing can be sealed.
pub async fn rig_without_sealing() -> Rig {
    build(false).await
}

async fn build(with_sealing: bool) -> Rig {
    let url = pg::pg_admin_url().expect("RUSTY_TEST_PG_URL");
    // The service runs as its own database role (deploy/db/roles.sql, grants_pg.sql): a query that needs more than that role has fails here.
    let db = pg::PgTestDb::create_with_roles(&url).await;
    let store = Store::open(StoreConfig::postgres(db.role_url("rusty_admin"))).await.unwrap();
    let outbound = Store::open(StoreConfig::postgres(db.role_url("rusty_app"))).await.unwrap();
    let dir = Directory::with_repo(store.repos().directory.clone());
    let admins = AdminStore::with_repo(store.repos().admin.clone());
    let now = Arc::new(ManualNow::new(1_000_000));
    let sessions = SessionStore::new(Arc::new(MemoryKv::new()), SessionConfig::default()).with_clock(now.clone());
    let throttle = Arc::new(AuthThrottle::new(1000, 600, 8, Arc::new(SystemClock::new())));
    let authenticator = Arc::new(LocalAuthenticator::new(dir.clone()).await.unwrap());
    let auth = AdminAuth::new(authenticator, dir.clone(), admins.clone(), sessions, throttle);
    let (unseal_key, seal_key) = rusty_seal::generate();
    let fx = rusty_store::fixtures::Fixtures::admin_of(&db, &store).await;
    let repos = store.repos();
    let services = Services {
        dns: rusty_store::DnsStore::with_repo(repos.dns.clone()),
        relays: rusty_store::RelayStore::with_repo(repos.relay.clone()),
        blob_stores: rusty_store::BlobStoreAdmin::with_repo(repos.blob_stores.clone()),
        pools: rusty_store::BlobPoolAdmin::with_repos(repos.blob_stores.clone(), repos.blob_pool.clone()),
        sealed: rusty_store::SealedWriter::with_repo(repos.sealed.clone()),
        jobs: rusty_store::JobQueue::with_repo(repos.jobs.clone()),
        sealing: with_sealing.then_some(seal_key),
        mail: Some(MailSettings { host: "mail.example.org".into(), public_ips: vec!["203.0.114.5".parse().unwrap()], spf_include: None }),
    };
    let app = Arc::new(AdminApp::new(auth, dir.clone(), admins.clone(), None, ORIGIN, SessionConfig::default()).with_services(services));
    let router = rusty_admin::autoconfig::routes(app.clone(), rusty_admin::dns::routes(app.clone(), outbound_routes(app.clone(), storage_routes(app.clone(), manage_routes(app.clone(), session_routes(app, Router::new()))))));
    let h = HttpHandler::new(router, HttpConfig::default());
    let spec = ListenerSpec { name: "admin-test", addr: "127.0.0.1:0".parse().unwrap(), max_connections: 64, max_per_ip: 64 };
    let bound = BoundListener::bind(spec).await.unwrap();
    let addr = bound.local_addr();
    let stop = CancellationToken::new();
    let s = stop.clone();
    tokio::spawn(async move {
        let _ = bound.serve(Arc::new(DirectTcp), Arc::new(h), Arc::new(TracingEvents), s, Duration::from_secs(5)).await;
    });
    Rig { addr, dir, admins, stop, now, unsealer: rusty_seal::Unsealer::new(vec![unseal_key]).unwrap(), fx, store, outbound, _db: db }
}

pub struct Reply {
    pub status: u16,
    pub head: String,
    pub body: String,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<String> {
        self.head.split("\r\n").skip(1).find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    }
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

pub async fn send(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
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

/// A signed-in administrator.
pub struct Client {
    pub addr: SocketAddr,
    pub cookie: String,
    pub csrf: String,
}

impl Client {
    pub async fn sign_in(addr: SocketAddr, address: &str) -> Client {
        let body = json!({ "address": address, "password": PW }).to_string();
        let r = send(addr, "POST", "/api/v1/session", &[("Content-Type", "application/json"), ("Origin", ORIGIN)], &body).await;
        assert_eq!(r.status, 200, "{address}: {} {}", r.head, r.body);
        let cookie = r.header("set-cookie").unwrap().split(';').next().unwrap().to_owned();
        Client { addr, cookie, csrf: r.json()["csrf"].as_str().unwrap().to_owned() }
    }

    pub async fn get(&self, path: &str) -> Reply {
        send(self.addr, "GET", path, &[("Cookie", &self.cookie)], "").await
    }

    pub async fn mutate(&self, method: &str, path: &str, body: Option<Value>) -> Reply {
        let text = body.map(|b| b.to_string()).unwrap_or_default();
        let h = [("Cookie", self.cookie.as_str()), ("Origin", ORIGIN), ("Content-Type", "application/json"), ("X-CSRF-Token", self.csrf.as_str())];
        send(self.addr, method, path, &h, &text).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Reply {
        self.mutate("POST", path, Some(body)).await
    }

    pub async fn post_empty(&self, path: &str) -> Reply {
        self.mutate("POST", path, None).await
    }
}

pub async fn make_admin(r: &Rig, address: &str, domain_scoped: bool) {
    let (local, domain) = address.split_once('@').unwrap();
    let d = match r.dir.find_domain(domain).await.unwrap() {
        Some(d) => d.id,
        None => r.dir.create_domain(domain).await.unwrap(),
    };
    let a = r.dir.create_account(local, d, Some(PW), None).await.unwrap();
    r.admins.grant(a, domain_scoped.then_some(d)).await.unwrap();
}

// ------------------------------------------------------------------------------------------------ the contract

pub fn contract() -> Value {
    serde_json::from_str(CONTRACT).unwrap()
}

/// `value` has exactly the fields and types of the contract shape `name`.
pub fn conforms(value: &Value, name: &str) {
    let c = contract();
    let shape = c["shapes"][name].as_object().unwrap_or_else(|| panic!("no shape {name}"));
    let obj = value.as_object().unwrap_or_else(|| panic!("{name}: not an object: {value}"));
    let got: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
    let want: BTreeSet<&str> = shape.keys().map(String::as_str).collect();
    assert_eq!(got, want, "{name}: fields differ: {value}");
    for (k, t) in shape {
        type_ok(&obj[k], t.as_str().unwrap(), &format!("{name}.{k}"));
    }
}

pub fn type_ok(v: &Value, t: &str, at: &str) {
    if let Some(inner) = t.strip_suffix("|null") {
        if v.is_null() {
            return;
        }
        return type_ok(v, inner, at);
    }
    if let Some(item) = t.strip_suffix("[]") {
        let a = v.as_array().unwrap_or_else(|| panic!("{at}: not an array: {v}"));
        for x in a {
            if item == "string" {
                assert!(x.is_string(), "{at}: {x}");
            } else {
                conforms(x, item);
            }
        }
        return;
    }
    let ok = match t {
        "integer" => v.is_i64(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        // Anything else names a shape of the contract.
        shape if shape.chars().next().is_some_and(|c| c.is_ascii_uppercase()) => {
            conforms(v, shape);
            true
        }
        _ => panic!("{at}: unknown type {t}"),
    };
    assert!(ok, "{at}: expected {t}, got {v}");
}

