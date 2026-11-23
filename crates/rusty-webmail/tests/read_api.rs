//! The read path of the webmail (docs/design-webmail.md section 4): folders with counts, a page of messages newest first by cursor, one message with its plain text and attachments, the original as a download; and
//! that a person reaches the mail of their own account and no other. The service runs as its own database role, against a real Postgres and a real blob directory.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rusty_core::config::{Role, StartupConfig};
use rusty_core::{FolderId, MessageId};
use rusty_directory::Directory;
use rusty_store::fixtures::pg_db as pg;
use rusty_store::{BlobRouter, BlobStores, FsBlobStore, FsStoreOpener, KindOpener, Mailbox, Store, StoreConfig};
use rusty_webmail::run::start;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

const ORIGIN: &str = "https://webmail.example.org";
const INBOX: FolderId = FolderId(1);

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| l.split_once(':').filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_owned()))
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

async fn call(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&str>) -> Reply {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    match body {
        Some(b) => req.push_str(&format!("Content-Length: {}\r\n\r\n{b}", b.len())),
        None => req.push_str("\r\n"),
    }
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    let at = out.windows(4).position(|w| w == b"\r\n\r\n").map_or(out.len(), |p| p + 4);
    let head = String::from_utf8_lossy(&out[..at.saturating_sub(4).min(out.len())]).into_owned();
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    Reply { status, head, body: out[at.min(out.len())..].to_vec() }
}

async fn sign_in(addr: SocketAddr, address: &str, password: &str) -> String {
    let body = serde_json::json!({ "address": address, "password": password }).to_string();
    let r = call(addr, "POST", "/api/v1/session", &[("Content-Type", "application/json"), ("Origin", ORIGIN)], Some(&body)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.header("set-cookie").unwrap().split(';').next().unwrap().to_owned()
}

fn router(store: &Store, dir: &std::path::Path, blobs: Arc<FsBlobStore>) -> Arc<dyn BlobRouter> {
    let opener = Arc::new(KindOpener::new().with_fs(Arc::new(FsStoreOpener::new(dir.join("stores")))));
    Arc::new(BlobStores::new(blobs, store.repos().blob_stores.clone(), opener))
}

struct Rig {
    addr: SocketAddr,
    stop: CancellationToken,
    alice: Mailbox,
    _db: pg::PgTestDb,
    _tmp: tempfile::TempDir,
}

async fn rig() -> Rig {
    let url = pg::pg_admin_url().expect("RUSTY_TEST_PG_URL");
    let db = pg::PgTestDb::create_with_roles(&url).await;
    let tmp = tempfile::tempdir().unwrap();
    let blob_dir = tmp.path().join("blobs");
    let service = Store::open(StoreConfig::postgres(db.role_url("rusty_webmail"))).await.unwrap();
    let operator = Store::open(StoreConfig::postgres(db.role_url("rusty_app"))).await.unwrap();
    let dir = Directory::with_repo(operator.repos().directory.clone());
    let d = dir.create_domain("one.test").await.unwrap();
    let alice_id = dir.create_account("alice", d, Some("correct-horse-battery-1"), None).await.unwrap();
    let bob_id = dir.create_account("bob", d, Some("another-long-password-2"), None).await.unwrap();
    FsBlobStore::init(&blob_dir).await.unwrap();
    let blobs = Arc::new(FsBlobStore::open(&blob_dir).await.unwrap());
    let stores = router(&operator, tmp.path(), blobs);
    let alice = Mailbox::open_with(operator.repos().mailbox.clone(), stores.clone(), alice_id).await.unwrap();
    let bob = Mailbox::open_with(operator.repos().mailbox.clone(), stores, bob_id).await.unwrap();
    let seen = rusty_core::Flags::SEEN;
    alice.append_dated(INBOX, Bytes::from_static(b"From: Bob <bob@one.test>\r\nSubject: first\r\nMessage-ID: <1@x>\r\n\r\none\r\n"), seen, 1_700_000_000).await.unwrap();
    alice.append_dated(INBOX, Bytes::from_static(b"From: =?utf-8?Q?Caf=C3=A9?= <c@x.test>\r\nSubject: =?utf-8?Q?Caf=C3=A9_menu?=\r\nMessage-ID: <2@x>\r\n\r\ntwo\r\n"), rusty_core::Flags::empty(), 1_700_000_100).await.unwrap();
    alice.append_dated(INBOX, Bytes::from_static(b"From: d@x.test\r\nSubject: with a file\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nSee the file.\r\n--b\r\nContent-Type: application/pdf; name=\"a.pdf\"\r\nContent-Disposition: attachment; filename=\"a.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nSGVsbG8gd29ybGQh\r\n--b--\r\n"), rusty_core::Flags::empty(), 1_700_000_200).await.unwrap();
    alice.append_dated(FolderId(2), Bytes::from_static(b"Subject: sent one\r\n\r\nsent\r\n"), seen, 1_700_000_300).await.unwrap();
    alice.append_dated(FolderId(3), Bytes::from_static(b"From: n@x.test
Subject: newsletter
Content-Type: text/html; charset=utf-8

<p>Hello <b>there</b></p><script>alert(1)</script><img src=\"https://track.example/p.gif\">
"), rusty_core::Flags::empty(), 1_700_000_400).await.unwrap();
    bob.append_dated(INBOX, Bytes::from_static(b"Subject: BOB-SECRET\r\n\r\nBOB-SECRET body\r\n"), rusty_core::Flags::empty(), 1_700_000_050).await.unwrap();
    let cfg = StartupConfig::from_toml_str_for(
        Role::Webmail,
        &format!(
            "blob_dir = \"{}\"\nhostname = \"webmail.example.org\"\n[store]\nurl = \"postgres://rusty_app@db.internal:5432/rusty\"\npassword_file = \"/run/secrets/pg\"\n[webmail]\nlisten = \"127.0.0.1:0\"\npublic_origin = \"{ORIGIN}\"\n",
            blob_dir.display()
        ),
    )
    .unwrap();
    let stop = CancellationToken::new();
    let r = start(&cfg, &service, stop.clone()).await.unwrap();
    Rig { addr: r.addr, stop, alice, _db: db, _tmp: tmp }
}

#[tokio::test]
async fn the_folders_a_page_newest_first_a_message_and_its_original_are_read_by_their_owner() {
    let r = rig().await;
    let cookie = sign_in(r.addr, "alice@one.test", "correct-horse-battery-1").await;
    let c = [("Cookie", cookie.as_str())];

    let folders = call(r.addr, "GET", "/api/v1/folders", &c, None).await;
    assert_eq!(folders.status, 200, "{}", folders.text());
    let items = folders.json()["items"].as_array().unwrap().clone();
    let inbox = items.iter().find(|f| f["role"] == "inbox").unwrap();
    assert_eq!((inbox["messages"].clone(), inbox["unseen"].clone()), (serde_json::json!(3), serde_json::json!(2)));
    assert!(items.iter().any(|f| f["role"] == "sent" && f["messages"] == 1));

    // Newest first, two at a time, by cursor.
    let page1 = call(r.addr, "GET", "/api/v1/folders/1/messages?limit=2", &c, None).await.json();
    let subjects: Vec<&str> = page1["items"].as_array().unwrap().iter().map(|i| i["subject"].as_str().unwrap()).collect();
    assert_eq!(subjects, ["with a file", "Café menu"], "newest first, the encoded subject decoded");
    assert_eq!(page1["items"][1]["from"], serde_json::json!("c@x.test"), "the list shows the sender address the store keeps");
    assert_eq!((page1["items"][0]["flags"]["seen"].clone(), page1["items"][2 - 1]["date"].clone()), (serde_json::json!(false), serde_json::json!(1_700_000_100)));
    let next = page1["next"].as_u64().expect("a cursor");
    let page2 = call(r.addr, "GET", &format!("/api/v1/folders/1/messages?limit=2&before={next}"), &c, None).await.json();
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert_eq!((page2["items"][0]["subject"].clone(), page2["next"].clone()), (serde_json::json!("first"), serde_json::Value::Null));
    assert_eq!(page2["items"][0]["flags"]["seen"], serde_json::json!(true));
    for bad in ["limit=0", "limit=101", "before=0", "before=x", "limit=2&extra=1"] {
        assert_eq!(call(r.addr, "GET", &format!("/api/v1/folders/1/messages?{bad}"), &c, None).await.status, 400, "{bad}");
    }
    assert_eq!(call(r.addr, "GET", "/api/v1/folders/99/messages", &c, None).await.status, 404);
    assert_eq!(call(r.addr, "GET", "/api/v1/folders/abc/messages", &c, None).await.status, 404);

    // One message: headers, the plain text, the attachments.
    let id = page1["items"][0]["id"].as_i64().unwrap();
    let m = call(r.addr, "GET", &format!("/api/v1/messages/{id}"), &c, None).await;
    assert_eq!(m.status, 200, "{}", m.text());
    let m = m.json();
    assert_eq!((m["subject"].clone(), m["from"].clone(), m["text"].clone()), (serde_json::json!("with a file"), serde_json::json!("d@x.test"), serde_json::json!("See the file.")));
    assert_eq!(m["attachments"], serde_json::json!([{ "name": "a.pdf", "content_type": "application/pdf", "size": 12 }]));
    assert_eq!((m["folder"].clone(), m["html_only"].clone()), (serde_json::json!(1), serde_json::json!(false)));

    // The original is a download, never a page of this origin.
    let raw = call(r.addr, "GET", &format!("/api/v1/messages/{id}/raw"), &c, None).await;
    assert_eq!(raw.status, 200);
    assert_eq!(raw.header("content-type").as_deref(), Some("application/octet-stream"));
    assert!(raw.header("content-disposition").unwrap().starts_with("attachment;"));
    assert_eq!(raw.header("x-content-type-options").as_deref(), Some("nosniff"));
    assert!(raw.header("content-security-policy").unwrap().contains("sandbox"));
    assert!(raw.text().contains("Content-Transfer-Encoding: base64"));
    r.stop.cancel();
}

#[tokio::test]
async fn a_person_reaches_the_mail_of_their_own_account_and_no_other_whatever_the_id_and_without_a_session_nothing() {
    let r = rig().await;
    // Nothing without a session.
    for path in ["/api/v1/folders", "/api/v1/folders/1/messages", "/api/v1/messages/1", "/api/v1/messages/1/raw"] {
        assert_eq!(call(r.addr, "GET", path, &[], None).await.status, 401, "{path}");
    }
    let cookie = sign_in(r.addr, "alice@one.test", "correct-horse-battery-1").await;
    let c = [("Cookie", cookie.as_str())];
    // Bob's message has an id inside Bob's mailbox; whatever id and folder Alice asks for, she gets her own or nothing, and never a word of Bob's.
    let mut seen_any = 0;
    for id in 0..12 {
        for suffix in ["", "/raw"] {
            let r = call(r.addr, "GET", &format!("/api/v1/messages/{id}{suffix}"), &c, None).await;
            assert!(!r.text().contains("BOB-SECRET"), "id {id}{suffix}");
            if r.status == 200 {
                seen_any += 1;
            } else {
                assert_eq!(r.status, 404, "id {id}{suffix}: {}", r.text());
            }
        }
    }
    assert!(seen_any >= 4, "her own messages are there");
    for folder in 1..10 {
        let page = call(r.addr, "GET", &format!("/api/v1/folders/{folder}/messages"), &c, None).await;
        assert!(!page.text().contains("BOB-SECRET"), "folder {folder}");
    }
    // Bob sees his own.
    let bob = sign_in(r.addr, "bob@one.test", "another-long-password-2").await;
    let page = call(r.addr, "GET", "/api/v1/folders/1/messages", &[("Cookie", bob.as_str())], None).await;
    assert!(page.text().contains("BOB-SECRET"));
    // A message the owner deleted is gone from the reader.
    r.alice.expunge(MessageId(1)).await.unwrap();
    assert_eq!(call(r.addr, "GET", "/api/v1/messages/1", &c, None).await.status, 404);
    r.stop.cancel();
}

#[tokio::test]
async fn the_html_of_a_message_is_served_only_for_a_sandboxed_frame_and_a_message_without_html_has_none() {
    let r = rig().await;
    let cookie = sign_in(r.addr, "alice@one.test", "correct-horse-battery-1").await;
    let c = [("Cookie", cookie.as_str())];
    let page = call(r.addr, "GET", "/api/v1/folders/3/messages", &c, None).await.json();
    let id = page["items"][0]["id"].as_i64().expect("the newsletter");
    // The reader is told there is HTML, and that there is no plain text.
    let m = call(r.addr, "GET", &format!("/api/v1/messages/{id}"), &c, None).await.json();
    assert_eq!((m["has_html"].clone(), m["html_only"].clone(), m["text"].clone()), (serde_json::json!(true), serde_json::json!(true), serde_json::json!("")));
    // The HTML itself carries the policy that makes it harmless: no script, nothing loaded, only this origin may frame it.
    let h = call(r.addr, "GET", &format!("/api/v1/messages/{id}/html"), &c, None).await;
    assert_eq!(h.status, 200);
    assert!(h.header("content-type").unwrap().starts_with("text/html"));
    let csp = h.header("content-security-policy").unwrap();
    assert!(csp.starts_with("sandbox ") && !csp.contains("allow-scripts") && csp.contains("default-src 'none'") && csp.contains("img-src data:") && csp.contains("frame-ancestors 'self'"), "{csp}");
    assert_eq!(h.header("x-content-type-options").as_deref(), Some("nosniff"));
    assert!(h.text().contains("<b>there</b>"));
    // An attachment is a download of unknown type, with a safe file name.
    let with_file = call(r.addr, "GET", "/api/v1/folders/1/messages?limit=1", &c, None).await.json()["items"][0]["id"].as_i64().unwrap();
    let file = call(r.addr, "GET", &format!("/api/v1/messages/{with_file}/attachments/0"), &c, None).await;
    assert_eq!(file.status, 200);
    assert_eq!((file.header("content-type").as_deref(), file.text().as_str()), (Some("application/octet-stream"), "Hello world!"));
    assert_eq!(file.header("content-disposition").as_deref(), Some("attachment; filename=\"a.pdf\"; filename*=UTF-8''a.pdf"));
    assert_eq!(file.header("x-content-type-options").as_deref(), Some("nosniff"));
    assert_eq!(call(r.addr, "GET", &format!("/api/v1/messages/{with_file}/attachments/1"), &c, None).await.status, 404);
    assert_eq!(call(r.addr, "GET", &format!("/api/v1/messages/{with_file}/attachments/x"), &c, None).await.status, 404);
    // A message with plain text only has no HTML to show, and nothing is served without a session.
    let plain = call(r.addr, "GET", "/api/v1/folders/1/messages", &c, None).await.json()["items"][0]["id"].as_i64().unwrap();
    assert_eq!(call(r.addr, "GET", &format!("/api/v1/messages/{plain}/html"), &c, None).await.status, 404);
    assert_eq!(call(r.addr, "GET", &format!("/api/v1/messages/{id}/html"), &[], None).await.status, 401);
    r.stop.cancel();
}
