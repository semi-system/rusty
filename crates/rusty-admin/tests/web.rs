//! The embedded front end served by the real service: the index for every client route, immutable hashed assets, the security headers, and a clean
//! JSON `404` for a mistyped API call. Skipped (with a note) when the binary was built without a front end, unless `RUSTY_ADMIN_WEB_REQUIRED=1` says it must
//! have one (the release and image builds).

use std::time::Duration;

use rusty_admin::run::start;
use rusty_core::config::{Role, StartupConfig};
use rusty_store::fixtures::pg_db as pg;
use rusty_store::{Store, StoreConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

const BASE: &str = "hostname = \"admin.example.org\"\n[store]\nurl = \"postgres://rusty_app@db.internal:5432/rusty\"\npassword_file = \"/run/secrets/pg\"\n[admin]\nlisten = \"127.0.0.1:0\"\npublic_origin = \"https://admin.example.org\"\n";

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.head.split("\r\n").skip(1).find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    }
}

async fn get(addr: std::net::SocketAddr, path: &str, extra: &str) -> Reply {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n{extra}\r\n").as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    Reply { status: head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0), head: format!("{head}\r\n"), body: body.to_owned() }
}

#[tokio::test]
async fn the_embedded_front_end_is_served_with_the_rules_of_the_design() {
    if !rusty_admin::web::is_embedded() {
        assert_ne!(std::env::var("RUSTY_ADMIN_WEB_REQUIRED").as_deref(), Ok("1"), "this build must embed the front end");
        eprintln!("no front end embedded: skipped");
        return;
    }
    let url = pg::pg_admin_url().expect("RUSTY_TEST_PG_URL");
    let db = pg::PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    let cfg = StartupConfig::from_toml_str_for(Role::Admin, BASE).unwrap();
    let stop = CancellationToken::new();
    let r = start(&cfg, &store, stop.clone()).await.unwrap();

    // The index answers `/` and every client-side route, revalidated; it has the headers of section 6.
    // A client route never ends in a name that may contain a dot (`/domains/example.org`): the asset layer answers a path that looks like a file with 404.
    for path in ["/", "/domains/example.org/overview", "/domains/example.org/dns"] {
        let page = get(r.addr, path, "").await;
        assert_eq!(page.status, 200, "{path}");
        assert!(page.header("content-type").unwrap().starts_with("text/html"), "{path}");
        assert_eq!(page.header("cache-control").as_deref(), Some("no-cache"), "{path}");
        assert!(page.body.contains("<div id=\"app\">"), "{path}");
        let csp = page.header("content-security-policy").unwrap();
        assert!(csp.contains("script-src 'self'") && csp.contains("default-src 'none'") && !csp.contains("unsafe-inline") && !csp.contains("unsafe-eval"), "{csp}");
        assert_eq!(page.header("x-content-type-options").as_deref(), Some("nosniff"));
    }

    // The page names its assets: they are served immutable, typed, and revalidated cheaply by their ETag.
    let index = get(r.addr, "/", "").await;
    let asset = index.body.split('"').find(|s| s.starts_with("/assets/") && s.ends_with(".js")).expect("a script in the index").to_owned();
    let js = get(r.addr, &asset, "").await;
    assert_eq!(js.status, 200);
    assert!(js.header("content-type").unwrap().starts_with("text/javascript"));
    assert!(js.header("cache-control").unwrap().contains("immutable"));
    let etag = js.header("etag").unwrap();
    assert_eq!(get(r.addr, &asset, &format!("If-None-Match: {etag}\r\n")).await.status, 304);

    // A mistyped API path is a clean JSON 404, never the index page; a missing file is a 404, not the index.
    let api = get(r.addr, "/api/v1/nothing", "").await;
    assert_eq!((api.status, api.body.as_str()), (404, "{\"error\":\"not_found\"}"));
    assert_eq!(get(r.addr, "/assets/missing.js", "").await.status, 404);
    // The API still works next to it.
    assert_eq!(get(r.addr, "/api/v1/session", "").await.status, 401);
    stop.cancel();
}
