//! The write path of the webmail, first part (docs/design-webmail.md section 5): flags, move, copy, delete, folders; every one needs the CSRF token and the Origin, and every one works on the mailbox of the
//! signed-in account only. The service runs as its own database role, against a real Postgres and a real blob directory.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rusty_core::config::{Role, StartupConfig};
use rusty_core::FolderId;
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
    let _ = tokio::time::timeout(Duration::from_secs(30), s.read_to_end(&mut out)).await;
    let at = out.windows(4).position(|w| w == b"\r\n\r\n").map_or(out.len(), |p| p + 4);
    let head = String::from_utf8_lossy(&out[..at.saturating_sub(4).min(out.len())]).into_owned();
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    Reply { status, head, body: out[at.min(out.len())..].to_vec() }
}

async fn sign_in(addr: SocketAddr, address: &str, password: &str) -> (String, String) {
    let body = serde_json::json!({ "address": address, "password": password }).to_string();
    let r = call(addr, "POST", "/api/v1/session", &[("Content-Type", "application/json"), ("Origin", ORIGIN)], Some(&body)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    (r.header("set-cookie").unwrap().split(';').next().unwrap().to_owned(), r.json()["csrf"].as_str().unwrap().to_owned())
}

fn router(store: &Store, dir: &std::path::Path, blobs: Arc<FsBlobStore>) -> Arc<dyn BlobRouter> {
    let opener = Arc::new(KindOpener::new().with_fs(Arc::new(FsStoreOpener::new(dir.join("stores")))));
    Arc::new(BlobStores::new(blobs, store.repos().blob_stores.clone(), opener))
}

struct Rig {
    addr: SocketAddr,
    stop: CancellationToken,
    operator: Store,
    alice_id: rusty_core::AccountId,
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
    bob.append_dated(INBOX, Bytes::from_static(b"Subject: BOB-SECRET\r\n\r\nBOB-SECRET body\r\n"), rusty_core::Flags::empty(), 1_700_000_050).await.unwrap();
    // The webmail's own key for the second factor.
    let key_file = tmp.path().join("totp.key");
    std::fs::write(&key_file, rusty_seal::generate().0.to_text().as_str()).unwrap();
    std::fs::set_permissions(&key_file, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let cfg = StartupConfig::from_toml_str_for(
        Role::Webmail,
        &format!(
            "blob_dir = \"{}\"\nhostname = \"webmail.example.org\"\n[store]\nurl = \"postgres://rusty_app@db.internal:5432/rusty\"\npassword_file = \"/run/secrets/pg\"\n[webmail]\nlisten = \"127.0.0.1:0\"\npublic_origin = \"{ORIGIN}\"\ntotp_key_file = \"{}\"\n",
            blob_dir.display(),
            key_file.display()
        ),
    )
    .unwrap();
    let stop = CancellationToken::new();
    let r = start(&cfg, &service, stop.clone()).await.unwrap();
    Rig { addr: r.addr, stop, operator, alice_id, _db: db, _tmp: tmp }
}


struct Session {
    cookie: String,
    csrf: String,
}

impl Session {
    async fn post(&self, addr: SocketAddr, path: &str, body: serde_json::Value) -> Reply {
        let b = body.to_string();
        call(addr, "POST", path, &[("Cookie", &self.cookie), ("Content-Type", "application/json"), ("Origin", ORIGIN), ("X-CSRF-Token", &self.csrf)], Some(&b)).await
    }
    async fn delete(&self, addr: SocketAddr, path: &str) -> Reply {
        call(addr, "DELETE", path, &[("Cookie", &self.cookie), ("Content-Type", "application/json"), ("Origin", ORIGIN), ("X-CSRF-Token", &self.csrf)], None).await
    }
    async fn get(&self, addr: SocketAddr, path: &str) -> serde_json::Value {
        call(addr, "GET", path, &[("Cookie", &self.cookie)], None).await.json()
    }
}

async fn alice(r: &Rig) -> Session {
    let (cookie, csrf) = sign_in(r.addr, "alice@one.test", "correct-horse-battery-1").await;
    Session { cookie, csrf }
}

fn folder(folders: &serde_json::Value, role: &str) -> i64 {
    folders["items"].as_array().unwrap().iter().find(|f| f["role"] == role).unwrap_or_else(|| panic!("no {role} folder"))["id"].as_i64().unwrap()
}

#[tokio::test]
async fn messages_are_marked_flagged_copied_moved_and_deleted_through_the_trash() {
    let r = rig().await;
    let s = alice(&r).await;
    let folders = s.get(r.addr, "/api/v1/folders").await;
    let (inbox, trash) = (folder(&folders, "inbox"), folder(&folders, "trash"));
    let page = s.get(r.addr, "/api/v1/folders/1/messages").await;
    let ids: Vec<i64> = page["items"].as_array().unwrap().iter().map(|i| i["id"].as_i64().unwrap()).collect();
    assert_eq!(ids.len(), 3);

    // Read, flag.
    let all = serde_json::json!(ids);
    let ok = s.post(r.addr, "/api/v1/messages/actions", serde_json::json!({ "ids": all, "action": "read" })).await;
    assert_eq!((ok.status, ok.json()["applied"].clone()), (200, serde_json::json!(3)), "{}", ok.text());
    let f = s.get(r.addr, "/api/v1/folders").await;
    assert_eq!(f["items"].as_array().unwrap().iter().find(|x| x["role"] == "inbox").unwrap()["unseen"], serde_json::json!(0));
    s.post(r.addr, "/api/v1/messages/actions", serde_json::json!({ "ids": [ids[0]], "action": "unread" })).await;
    s.post(r.addr, "/api/v1/messages/actions", serde_json::json!({ "ids": [ids[0]], "action": "flag" })).await;
    let m = s.get(r.addr, &format!("/api/v1/messages/{}", ids[0])).await;
    assert_eq!((m["flags"]["seen"].clone(), m["flags"]["flagged"].clone()), (serde_json::json!(false), serde_json::json!(true)));
    s.post(r.addr, "/api/v1/messages/actions", serde_json::json!({ "ids": [ids[0]], "action": "unflag" })).await;

    // A new folder, a copy and a move into it.
    let created = s.post(r.addr, "/api/v1/folders", serde_json::json!({ "name": "Receipts" })).await;
    assert_eq!(created.status, 200, "{}", created.text());
    let receipts = created.json()["id"].as_i64().unwrap();
    let copy = s.post(r.addr, "/api/v1/messages/actions", serde_json::json!({ "ids": [ids[1]], "action": "copy", "folder": receipts })).await;
    assert_eq!((copy.status, copy.json()["applied"].clone()), (200, serde_json::json!(1)), "{}", copy.text());
    let mv = s.post(r.addr, "/api/v1/messages/actions", serde_json::json!({ "ids": [ids[2]], "action": "move", "folder": receipts })).await;
    assert_eq!(mv.json()["applied"], serde_json::json!(1));
    assert_eq!(s.get(r.addr, &format!("/api/v1/folders/{receipts}/messages")).await["items"].as_array().unwrap().len(), 2);
    assert_eq!(s.get(r.addr, &format!("/api/v1/folders/{inbox}/messages")).await["items"].as_array().unwrap().len(), 2);

    // Delete goes to the Trash first, and removes for good from there.
    let del = s.post(r.addr, "/api/v1/messages/actions", serde_json::json!({ "ids": [ids[0]], "action": "delete" })).await;
    assert_eq!((del.status, del.json()["applied"].clone()), (200, serde_json::json!(1)), "{}", del.text());
    assert_eq!(s.get(r.addr, &format!("/api/v1/folders/{trash}/messages")).await["items"].as_array().unwrap().len(), 1);
    let again = s.post(r.addr, "/api/v1/messages/actions", serde_json::json!({ "ids": [ids[0]], "action": "delete" })).await;
    assert_eq!(again.json()["applied"], serde_json::json!(1));
    assert_eq!(s.get(r.addr, &format!("/api/v1/folders/{trash}/messages")).await["items"].as_array().unwrap().len(), 0);
    assert_eq!(call(r.addr, "GET", &format!("/api/v1/messages/{}", ids[0]), &[("Cookie", &s.cookie)], None).await.status, 404);

    // Emptying works for the Trash and Junk only.
    s.post(r.addr, "/api/v1/messages/actions", serde_json::json!({ "ids": [ids[1]], "action": "delete" })).await;
    assert_eq!(s.post(r.addr, &format!("/api/v1/folders/{inbox}/empty"), serde_json::json!({})).await.status, 400);
    let e = s.post(r.addr, &format!("/api/v1/folders/{trash}/empty"), serde_json::json!({})).await;
    assert_eq!((e.status, e.json()["removed"].clone()), (200, serde_json::json!(1)), "{}", e.text());

    // Rename and delete a folder of her own; a system folder stays.
    assert_eq!(s.post(r.addr, &format!("/api/v1/folders/{receipts}/rename"), serde_json::json!({ "name": "Bills" })).await.status, 200);
    let f = s.get(r.addr, "/api/v1/folders").await;
    assert!(f["items"].as_array().unwrap().iter().any(|x| x["name"] == "Bills"));
    assert_eq!(s.delete(r.addr, &format!("/api/v1/folders/{receipts}")).await.status, 409, "a folder with messages is not deleted");
    let spare = s.post(r.addr, "/api/v1/folders", serde_json::json!({ "name": "Spare" })).await.json()["id"].as_i64().unwrap();
    assert_eq!(s.delete(r.addr, &format!("/api/v1/folders/{spare}")).await.status, 200);
    assert!(s.delete(r.addr, &format!("/api/v1/folders/{inbox}")).await.status >= 400, "a system folder is refused");
    r.stop.cancel();
}

#[tokio::test]
async fn a_write_needs_the_token_and_the_origin_and_never_touches_another_account() {
    let r = rig().await;
    let s = alice(&r).await;
    let body = serde_json::json!({ "ids": [1], "action": "read" }).to_string();
    let base = [("Cookie", s.cookie.as_str()), ("Content-Type", "application/json")];
    let no_origin = call(r.addr, "POST", "/api/v1/messages/actions", &[base[0], base[1], ("X-CSRF-Token", &s.csrf)], Some(&body)).await;
    let no_token = call(r.addr, "POST", "/api/v1/messages/actions", &[base[0], base[1], ("Origin", ORIGIN)], Some(&body)).await;
    let wrong_token = call(r.addr, "POST", "/api/v1/messages/actions", &[base[0], base[1], ("Origin", ORIGIN), ("X-CSRF-Token", "nope")], Some(&body)).await;
    assert_eq!((no_origin.status, no_token.status, wrong_token.status), (403, 403, 403));
    let anon = call(r.addr, "POST", "/api/v1/messages/actions", &[("Content-Type", "application/json"), ("Origin", ORIGIN)], Some(&body)).await;
    assert_eq!(anon.status, 401);
    // Bad bodies.
    for bad in [
        serde_json::json!({ "ids": [], "action": "read" }),
        serde_json::json!({ "ids": [0], "action": "read" }),
        serde_json::json!({ "ids": (1..=101).collect::<Vec<i64>>(), "action": "read" }),
        serde_json::json!({ "ids": [1], "action": "explode" }),
        serde_json::json!({ "ids": [1], "action": "move" }),
        serde_json::json!({ "ids": [1], "action": "read", "folder": 1 }),
        serde_json::json!({ "ids": [1], "action": "read", "extra": 1 }),
    ] {
        assert_eq!(s.post(r.addr, "/api/v1/messages/actions", bad.clone()).await.status, 400, "{bad}");
    }
    // Bob's mail: ids that exist only in his mailbox are missing for Alice and nothing of his changes.
    let (bob_cookie, _) = sign_in(r.addr, "bob@one.test", "another-long-password-2").await;
    let before = call(r.addr, "GET", "/api/v1/folders/1/messages", &[("Cookie", &bob_cookie)], None).await.json();
    let mut hit = 0;
    for id in 1..=12 {
        for action in ["read", "flag", "delete"] {
            let res = s.post(r.addr, "/api/v1/messages/actions", serde_json::json!({ "ids": [id], "action": action })).await;
            assert_eq!(res.status, 200, "{}", res.text());
            hit += res.json()["applied"].as_i64().unwrap();
        }
    }
    assert!(hit > 0, "her own messages were reached");
    let after = call(r.addr, "GET", "/api/v1/folders/1/messages", &[("Cookie", &bob_cookie)], None).await.json();
    assert_eq!(before, after, "Bob's mailbox is unchanged");
    assert!(after.to_string().contains("BOB-SECRET"));
    r.stop.cancel();
}

#[tokio::test]
async fn a_message_is_queued_in_the_outbox_of_the_sender_and_read_back_by_them_alone() {
    let r = rig().await;
    let s = alice(&r).await;
    let sent = s
        .post(r.addr, "/api/v1/send", serde_json::json!({ "to": ["Bob@One.Test", "far@elsewhere.example"], "bcc": ["quiet@one.test"], "subject": "Café", "text": "Hello\r\nWorld" }))
        .await;
    assert_eq!(sent.status, 202, "{}", sent.text());
    let id = sent.json()["id"].as_i64().unwrap();
    let outbox = s.get(r.addr, "/api/v1/outbox").await;
    let item = &outbox["items"][0];
    assert_eq!((item["id"].clone(), item["state"].clone(), item["subject"].clone()), (serde_json::json!(id), serde_json::json!("queued"), serde_json::json!("Café")));
    assert_eq!(item["recipients"], serde_json::json!(["bob@one.test", "far@elsewhere.example", "quiet@one.test"]));
    assert!(!outbox.to_string().contains("Hello"), "the text of the message is not in the list");

    // Bob sees nothing of it, and cannot withdraw it.
    let (bob_cookie, bob_csrf) = sign_in(r.addr, "bob@one.test", "another-long-password-2").await;
    let bob = Session { cookie: bob_cookie, csrf: bob_csrf };
    assert_eq!(bob.get(r.addr, "/api/v1/outbox").await["items"].as_array().unwrap().len(), 0);
    assert_eq!(bob.delete(r.addr, &format!("/api/v1/outbox/{id}")).await.status, 404);
    // The owner withdraws it.
    assert_eq!(s.delete(r.addr, &format!("/api/v1/outbox/{id}")).await.status, 200);
    assert_eq!(s.delete(r.addr, &format!("/api/v1/outbox/{id}")).await.status, 404);
    assert_eq!(s.get(r.addr, "/api/v1/outbox").await["items"].as_array().unwrap().len(), 0);
    r.stop.cancel();
}

#[tokio::test]
async fn a_send_needs_the_token_and_refuses_what_could_add_a_header_or_has_no_recipient() {
    let r = rig().await;
    let s = alice(&r).await;
    let body = serde_json::json!({ "to": ["bob@one.test"], "subject": "x", "text": "y" }).to_string();
    let headers = [("Cookie", s.cookie.as_str()), ("Content-Type", "application/json"), ("Origin", ORIGIN)];
    assert_eq!(call(r.addr, "POST", "/api/v1/send", &headers, Some(&body)).await.status, 403, "no token");
    assert_eq!(call(r.addr, "POST", "/api/v1/send", &[("Content-Type", "application/json"), ("Origin", ORIGIN)], Some(&body)).await.status, 401);
    assert_eq!(call(r.addr, "GET", "/api/v1/outbox", &[], None).await.status, 401);
    for (bad, code) in [
        (serde_json::json!({ "to": [], "text": "y" }), "no_recipient"),
        (serde_json::json!({ "to": ["bob@one.test\r\nBcc: evil@x.test"], "text": "y" }), "bad_address"),
        (serde_json::json!({ "to": ["not-an-address"], "text": "y" }), "bad_address"),
        (serde_json::json!({ "to": ["bob@one.test"], "subject": "a\r\nBcc: evil@x.test", "text": "y" }), "invalid"),
        (serde_json::json!({ "to": ["bob@one.test"], "in_reply_to": "x", "text": "y" }), "invalid"),
        (serde_json::json!({ "to": ["bob@one.test"], "from": "bob@one.test", "text": "y" }), "invalid"),
        (serde_json::json!({ "to": (0..101).map(|i| format!("u{i}@x.test")).collect::<Vec<_>>(), "text": "y" }), "too_many_recipients"),
    ] {
        let res = s.post(r.addr, "/api/v1/send", bad.clone()).await;
        assert_eq!((res.status, res.json()["error"].clone()), (400, serde_json::json!(code)), "{bad}: {}", res.text());
    }
    // A message over the size limit.
    let big = "x".repeat(2 << 20);
    assert_eq!(s.post(r.addr, "/api/v1/send", serde_json::json!({ "to": ["bob@one.test"], "text": big })).await.status, 413);
    assert_eq!(s.get(r.addr, "/api/v1/outbox").await["items"].as_array().unwrap().len(), 0, "nothing of that was queued");
    // The outbox of one account holds at most fifty waiting messages.
    for _ in 0..50 {
        assert_eq!(s.post(r.addr, "/api/v1/send", serde_json::json!({ "to": ["bob@one.test"], "text": "y" })).await.status, 202);
    }
    assert_eq!(s.post(r.addr, "/api/v1/send", serde_json::json!({ "to": ["bob@one.test"], "text": "y" })).await.status, 429);
    r.stop.cancel();
}

#[tokio::test]
async fn an_app_password_is_made_with_the_password_shown_once_works_for_the_protocols_only_and_is_revoked() {
    use rusty_directory::{Authenticator, LocalAuthenticator};
    let r = rig().await;
    let s = alice(&r).await;
    // Making one needs the account's password again.
    let no = s.post(r.addr, "/api/v1/app-passwords", serde_json::json!({ "label": "Phone", "password": "wrong-password-xx" })).await;
    assert_eq!(no.status, 401, "{}", no.text());
    assert_eq!(s.post(r.addr, "/api/v1/app-passwords", serde_json::json!({ "label": "", "password": "correct-horse-battery-1" })).await.status, 400);
    let made = s.post(r.addr, "/api/v1/app-passwords", serde_json::json!({ "label": "Phone", "password": "correct-horse-battery-1" })).await;
    assert_eq!(made.status, 200, "{}", made.text());
    let (id, secret) = (made.json()["id"].as_i64().unwrap(), made.json()["secret"].as_str().unwrap().to_owned());
    assert_eq!(secret.len(), 19);
    // The list never has the secret.
    let list = s.get(r.addr, "/api/v1/app-passwords").await;
    assert_eq!(list["items"][0]["label"], serde_json::json!("Phone"));
    assert!(!list.to_string().contains(&secret));
    // The mail protocols accept it, the webmail does not, and Bob has none.
    let operator = Store::open(StoreConfig::postgres(r._db.role_url("rusty_app"))).await.unwrap();
    let dir = Directory::with_repo(operator.repos().directory.clone());
    let protocols = LocalAuthenticator::new(dir.clone()).await.unwrap().with_app_passwords(operator.repos().app_passwords.clone());
    let own = LocalAuthenticator::new(dir).await.unwrap();
    assert!(protocols.verify("alice@one.test", &secret).await.unwrap().is_some());
    assert!(protocols.verify("bob@one.test", &secret).await.unwrap().is_none());
    assert!(own.verify("alice@one.test", &secret).await.unwrap().is_none(), "the webmail does not accept it");
    let body = serde_json::json!({ "address": "alice@one.test", "password": secret }).to_string();
    let attempt = call(r.addr, "POST", "/api/v1/session", &[("Content-Type", "application/json"), ("Origin", ORIGIN)], Some(&body)).await;
    assert_eq!(attempt.status, 401);
    // Its use is noted, and revoking it ends it; another account cannot revoke it.
    let (bob_cookie, bob_csrf) = sign_in(r.addr, "bob@one.test", "another-long-password-2").await;
    let bob = Session { cookie: bob_cookie, csrf: bob_csrf };
    assert_eq!(bob.delete(r.addr, &format!("/api/v1/app-passwords/{id}")).await.status, 404);
    assert_eq!(s.delete(r.addr, &format!("/api/v1/app-passwords/{id}")).await.status, 200);
    assert!(protocols.verify("alice@one.test", &secret).await.unwrap().is_none());
    // At most ten.
    for i in 0..10 {
        let res = s.post(r.addr, "/api/v1/app-passwords", serde_json::json!({ "label": format!("d{i}"), "password": "correct-horse-battery-1" })).await;
        assert_eq!(res.status, 200, "{}", res.text());
    }
    assert_eq!(s.post(r.addr, "/api/v1/app-passwords", serde_json::json!({ "label": "one more", "password": "correct-horse-battery-1" })).await.status, 429);
    r.stop.cancel();
}

#[tokio::test]
async fn a_draft_is_kept_in_the_drafts_folder_replaced_by_the_next_save_read_back_and_removed_when_sent() {
    let r = rig().await;
    let s = alice(&r).await;
    let folders = s.get(r.addr, "/api/v1/folders").await;
    let drafts = folder(&folders, "drafts");
    let first = s.post(r.addr, "/api/v1/drafts", serde_json::json!({ "to": ["bob@one.test"], "bcc": ["quiet@one.test"], "subject": "Plan", "text": "half a thought" })).await;
    assert_eq!(first.status, 200, "{}", first.text());
    let id1 = first.json()["id"].as_i64().unwrap();
    // A draft may have no recipient yet; saving again replaces it.
    let second = s.post(r.addr, "/api/v1/drafts", serde_json::json!({ "id": id1, "subject": "Plan", "text": "a whole thought" })).await;
    let id2 = second.json()["id"].as_i64().unwrap();
    assert_ne!(id1, id2);
    let page = s.get(r.addr, &format!("/api/v1/folders/{drafts}/messages")).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1, "the first draft was replaced");
    assert_eq!(page["items"][0]["flags"]["draft"], serde_json::json!(true));
    let m = s.get(r.addr, &format!("/api/v1/messages/{id2}")).await;
    assert_eq!((m["subject"].clone(), m["text"].clone(), m["folder"].clone()), (serde_json::json!("Plan"), serde_json::json!("a whole thought"), serde_json::json!(drafts)));
    // Sending from the draft queues the message and removes the draft; an id that is not a draft is never removed.
    let inbox_msg = s.get(r.addr, "/api/v1/folders/1/messages").await["items"][0]["id"].as_i64().unwrap();
    let sent = s.post(r.addr, "/api/v1/send", serde_json::json!({ "to": ["bob@one.test"], "subject": "Plan", "text": "x", "draft_id": inbox_msg })).await;
    assert_eq!(sent.status, 202);
    assert_eq!(call(r.addr, "GET", &format!("/api/v1/messages/{inbox_msg}"), &[("Cookie", &s.cookie)], None).await.status, 200, "a message outside Drafts stays");
    let sent = s.post(r.addr, "/api/v1/send", serde_json::json!({ "to": ["bob@one.test"], "subject": "Plan", "text": "a whole thought", "draft_id": id2 })).await;
    assert_eq!(sent.status, 202);
    assert_eq!(s.get(r.addr, &format!("/api/v1/folders/{drafts}/messages")).await["items"].as_array().unwrap().len(), 0);
    // Bad input is refused like a send, and nothing is needed to save but a session and the token.
    assert_eq!(s.post(r.addr, "/api/v1/drafts", serde_json::json!({ "to": ["no at sign"] })).await.status, 400);
    assert_eq!(call(r.addr, "POST", "/api/v1/drafts", &[("Content-Type", "application/json"), ("Origin", ORIGIN)], Some("{}")).await.status, 401);
    r.stop.cancel();
}

#[tokio::test]
async fn the_signature_is_kept_per_account_read_back_and_refused_when_it_is_markup_of_the_wrong_kind_or_too_long() {
    let r = rig().await;
    let s = alice(&r).await;
    assert_eq!(s.get(r.addr, "/api/v1/settings").await["signature"], serde_json::json!(""));
    let put = |sig: serde_json::Value| {
        let b = serde_json::json!({ "signature": sig }).to_string();
        let headers = [("Cookie", s.cookie.clone()), ("Content-Type", "application/json".to_owned()), ("Origin", ORIGIN.to_owned()), ("X-CSRF-Token", s.csrf.clone())];
        let addr = r.addr;
        async move {
            let h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            call(addr, "PUT", "/api/v1/settings", &h, Some(&b)).await
        }
    };
    let ok = put(serde_json::json!("Alice\r\nOne Test")).await;
    assert_eq!((ok.status, ok.json()["signature"].clone()), (200, serde_json::json!("Alice\nOne Test")), "{}", ok.text());
    assert_eq!(s.get(r.addr, "/api/v1/settings").await["signature"], serde_json::json!("Alice\nOne Test"));
    assert_eq!(put(serde_json::json!("x".repeat(2001))).await.status, 400);
    assert_eq!(put(serde_json::json!("bell\u{7}")).await.status, 400);
    // Bob has his own, and a write needs the token.
    let (bob_cookie, _) = sign_in(r.addr, "bob@one.test", "another-long-password-2").await;
    assert_eq!(call(r.addr, "GET", "/api/v1/settings", &[("Cookie", &bob_cookie)], None).await.json()["signature"], serde_json::json!(""));
    let body = serde_json::json!({ "signature": "x" }).to_string();
    assert_eq!(call(r.addr, "PUT", "/api/v1/settings", &[("Cookie", &s.cookie), ("Content-Type", "application/json"), ("Origin", ORIGIN)], Some(&body)).await.status, 403);
    r.stop.cancel();
}

#[tokio::test]
async fn the_second_factor_is_begun_confirmed_asked_at_sign_in_once_per_code_and_turned_off() {
    use rusty_directory::{Authenticator, LocalAuthenticator};
    use rusty_webmail::totp;
    let r = rig().await;
    let s = alice(&r).await;
    let status = s.get(r.addr, "/api/v1/totp").await;
    assert_eq!((status["available"].clone(), status["enabled"].clone()), (serde_json::json!(true), serde_json::json!(false)));
    // Begin needs the password again.
    assert_eq!(s.post(r.addr, "/api/v1/totp/begin", serde_json::json!({ "password": "wrong-password-xx" })).await.status, 401);
    let begun = s.post(r.addr, "/api/v1/totp/begin", serde_json::json!({ "password": "correct-horse-battery-1" })).await;
    assert_eq!(begun.status, 200, "{}", begun.text());
    let secret = totp::base32_decode(begun.json()["secret"].as_str().unwrap()).unwrap();
    assert!(begun.json()["uri"].as_str().unwrap().starts_with("otpauth://totp/Rusty%20mail:alice%40one.test?secret="));
    assert_eq!(s.get(r.addr, "/api/v1/totp").await["pending"], serde_json::json!(true));
    // A wrong code does not turn it on; the right one does and gives ten recovery codes, once.
    let now = rusty_webmail::compose::now() as u64;
    let step = totp::step_of(now);
    assert_eq!(s.post(r.addr, "/api/v1/totp/confirm", serde_json::json!({ "code": "000000" })).await.status, 401);
    let ok = s.post(r.addr, "/api/v1/totp/confirm", serde_json::json!({ "code": format!("{:06}", totp::code(&secret, step)) })).await;
    assert_eq!(ok.status, 200, "{}", ok.text());
    let recovery: Vec<String> = ok.json()["recovery"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_owned()).collect();
    assert_eq!(recovery.len(), 10);
    assert_eq!(s.get(r.addr, "/api/v1/totp").await["enabled"], serde_json::json!(true));
    // Sign-in now wants a code: none, a wrong one, then a right one of the next step (the step of the confirmation is used).
    let attempt = |code: Option<String>| {
        let mut body = serde_json::json!({ "address": "alice@one.test", "password": "correct-horse-battery-1" });
        if let Some(c) = code {
            body["code"] = serde_json::json!(c);
        }
        let b = body.to_string();
        let addr = r.addr;
        async move { call(addr, "POST", "/api/v1/session", &[("Content-Type", "application/json"), ("Origin", ORIGIN)], Some(&b)).await }
    };
    let none = attempt(None).await;
    assert_eq!((none.status, none.json()["error"].clone()), (401, serde_json::json!("second_factor")));
    assert_eq!(attempt(Some("000000".into())).await.json()["error"], serde_json::json!("second_factor"));
    assert_eq!(attempt(Some(format!("{:06}", totp::code(&secret, step)))).await.status, 401, "the step of the confirmation cannot be used again");
    let next = attempt(Some(format!("{:06}", totp::code(&secret, step + 1)))).await;
    assert_eq!(next.status, 200, "{}", next.text());
    assert_eq!(attempt(Some(format!("{:06}", totp::code(&secret, step + 1)))).await.status, 401, "a code works once");
    // A wrong password is still the plain 401, whatever the code.
    let bad = serde_json::json!({ "address": "alice@one.test", "password": "nope-nope-nope-1", "code": "123456" }).to_string();
    assert_eq!(call(r.addr, "POST", "/api/v1/session", &[("Content-Type", "application/json"), ("Origin", ORIGIN)], Some(&bad)).await.json()["error"], serde_json::json!("unauthorized"));
    // A recovery code stands in for a code, once.
    let by_recovery = attempt(Some(recovery[0].clone())).await;
    assert_eq!(by_recovery.status, 200, "{}", by_recovery.text());
    assert_eq!(attempt(Some(recovery[0].clone())).await.status, 401);
    // The mail protocols refuse the own password of this account now, and accept an app password.
    let operator = Store::open(StoreConfig::postgres(r._db.role_url("rusty_app"))).await.unwrap();
    let dir = Directory::with_repo(operator.repos().directory.clone());
    let protocols = LocalAuthenticator::new(dir).await.unwrap().with_app_passwords(operator.repos().app_passwords.clone()).with_second_factor_check(operator.repos().totp.clone());
    assert!(protocols.verify("alice@one.test", "correct-horse-battery-1").await.unwrap().is_none(), "the own password is refused on the protocols");
    assert!(protocols.verify("bob@one.test", "another-long-password-2").await.unwrap().is_some(), "another account is not affected");
    let made = s.post(r.addr, "/api/v1/app-passwords", serde_json::json!({ "label": "Phone", "password": "correct-horse-battery-1" })).await.json();
    assert!(protocols.verify("alice@one.test", made["secret"].as_str().unwrap()).await.unwrap().is_some());
    // Turning it off needs the password and a code; then everything is as before.
    assert_eq!(s.post(r.addr, "/api/v1/totp/disable", serde_json::json!({ "password": "correct-horse-battery-1", "code": "000000" })).await.status, 401);
    let off = s.post(r.addr, "/api/v1/totp/disable", serde_json::json!({ "password": "correct-horse-battery-1", "code": recovery[1].clone() })).await;
    assert_eq!(off.status, 200, "{}", off.text());
    assert_eq!(attempt(None).await.status, 200);
    assert!(protocols.verify("alice@one.test", "correct-horse-battery-1").await.unwrap().is_some());
    r.stop.cancel();
}

#[tokio::test]
async fn filters_are_checked_by_the_parser_kept_per_account_activated_one_at_a_time_and_removed_when_not_active() {
    let r = rig().await;
    let s = alice(&r).await;
    let put = |name: &str, body: &str| {
        let b = serde_json::json!({ "body": body }).to_string();
        let path = format!("/api/v1/sieve/{name}");
        let headers = [("Cookie", s.cookie.clone()), ("Content-Type", "application/json".to_owned()), ("Origin", ORIGIN.to_owned()), ("X-CSRF-Token", s.csrf.clone())];
        let addr = r.addr;
        async move {
            let h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            call(addr, "PUT", &path, &h, Some(&b)).await
        }
    };
    // A script the parser refuses is not kept, and the answer says where.
    let bad = put("main", "require \"fileinto\";\nif header :contains \"subject\" \"x\" {\n").await;
    assert_eq!(bad.status, 400);
    assert_eq!((bad.json()["error"].clone(), bad.json()["line"].clone()), (serde_json::json!("script_invalid"), serde_json::json!(2)), "{}", bad.text());
    assert!(s.get(r.addr, "/api/v1/sieve").await["scripts"].as_array().unwrap().is_empty());
    // A good one is kept and read back; saving again replaces it.
    let good = "require \"fileinto\";\nif header :contains \"list-id\" \"lists.example.org\" { fileinto \"Lists\"; }\n";
    assert_eq!(put("main", good).await.status, 200);
    assert_eq!(put("main", &format!("{good}stop;\n")).await.status, 200);
    let got = s.get(r.addr, "/api/v1/sieve/main").await;
    assert_eq!(got["body"], serde_json::json!(format!("{good}stop;\n")));
    // One runs at a time; an active one is not removed.
    assert_eq!(s.post(r.addr, "/api/v1/sieve/main/activate", serde_json::json!({})).await.status, 200);
    assert_eq!(put("second", "keep;").await.status, 200);
    assert_eq!(s.post(r.addr, "/api/v1/sieve/second/activate", serde_json::json!({})).await.status, 200);
    let list = s.get(r.addr, "/api/v1/sieve").await;
    let active: Vec<&str> = list["scripts"].as_array().unwrap().iter().filter(|x| x["active"] == true).map(|x| x["name"].as_str().unwrap()).collect();
    assert_eq!(active, ["second"]);
    assert_eq!(s.delete(r.addr, "/api/v1/sieve/second").await.status, 409);
    assert_eq!(s.post(r.addr, "/api/v1/sieve/deactivate", serde_json::json!({})).await.status, 200);
    assert_eq!(s.delete(r.addr, "/api/v1/sieve/second").await.status, 200);
    assert_eq!(s.delete(r.addr, "/api/v1/sieve/second").await.status, 404);
    assert_eq!(s.post(r.addr, "/api/v1/sieve/nothing/activate", serde_json::json!({})).await.status, 404);
    // Names, size and number are bounded.
    assert!([400, 404].contains(&put("bad%2Fname", "keep;").await.status), "an encoded slash is no name");
    assert_eq!(put("a%3Cb", "keep;").await.status, 404, "nor is anything the name check refuses");
    assert_eq!(put("x", &"#".repeat(70_000)).await.status, 413);
    for i in 0..9 {
        assert_eq!(put(&format!("s{i}"), "keep;").await.status, 200);
    }
    assert_eq!(put("one-too-many", "keep;").await.status, 429);
    // Another account sees none of it; a write needs the token.
    let (bob_cookie, _) = sign_in(r.addr, "bob@one.test", "another-long-password-2").await;
    assert!(call(r.addr, "GET", "/api/v1/sieve", &[("Cookie", &bob_cookie)], None).await.json()["scripts"].as_array().unwrap().is_empty());
    assert_eq!(call(r.addr, "GET", "/api/v1/sieve/main", &[("Cookie", &bob_cookie)], None).await.status, 404);
    let body = serde_json::json!({ "body": "keep;" }).to_string();
    assert_eq!(call(r.addr, "PUT", "/api/v1/sieve/main", &[("Cookie", &s.cookie), ("Content-Type", "application/json"), ("Origin", ORIGIN)], Some(&body)).await.status, 403);
    r.stop.cancel();
}

#[tokio::test]
async fn the_composer_is_offered_the_contacts_of_the_person_from_their_address_books_and_nobody_elses() {
    let r = rig().await;
    // The contacts are kept through CardDAV (the operator's role stands in for the DAV service).
    let dav = r.operator.repos().dav.clone();
    dav.ensure_defaults(r.alice_id).await.unwrap();
    let book = dav.collection(r.alice_id, "addressbook", "default").await.unwrap().unwrap().id;
    for (name, uid, fnm, mail) in [("ada.vcf", "ada", "Ada Lovelace", "ada@example.org"), ("alan.vcf", "alan", "Alan Turing", "alan@example.org")] {
        let body = format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:{fnm}\r\nEMAIL:{mail}\r\nEND:VCARD\r\n");
        let new = rusty_store::NewDavObject { name: name.into(), uid: uid.into(), body, component: "VCARD".into(), starts_at: None, ends_at: None, recurring: false };
        dav.put(book, &new, &rusty_store::DavPrecondition::None).await.unwrap();
    }
    let s = alice(&r).await;
    let by = |q: &'static str| {
        let (addr, cookie) = (r.addr, s.cookie.clone());
        async move { call(addr, "GET", &format!("/api/v1/contacts?q={q}"), &[("Cookie", &cookie)], None).await }
    };
    let ada = by("ada").await;
    assert_eq!(ada.status, 200, "{}", ada.text());
    assert_eq!(ada.json()["items"], serde_json::json!([{ "name": "Ada Lovelace", "email": "ada@example.org" }]));
    let both = by("example.org").await;
    assert_eq!(both.json()["items"].as_array().unwrap().len(), 2);
    assert!(by("zzz").await.json()["items"].as_array().unwrap().is_empty());
    // A pattern character is itself: `%%` finds nothing.
    assert!(by("%25%25").await.json()["items"].as_array().unwrap().is_empty());
    // Too short, and nothing without a session.
    assert_eq!(by("a").await.status, 400);
    assert_eq!(call(r.addr, "GET", "/api/v1/contacts?q=ada", &[], None).await.status, 401);
    assert_eq!(call(r.addr, "GET", "/api/v1/contacts", &[("Cookie", &s.cookie)], None).await.status, 400);
    // Another person sees none of it.
    let (bob_cookie, _) = sign_in(r.addr, "bob@one.test", "another-long-password-2").await;
    assert!(call(r.addr, "GET", "/api/v1/contacts?q=ada", &[("Cookie", &bob_cookie)], None).await.json()["items"].as_array().unwrap().is_empty());
    r.stop.cancel();
}

#[tokio::test]
async fn the_contacts_of_the_person_are_exported_as_one_vcf_file_and_nobody_elses() {
    let r = rig().await;
    let dav = r.operator.repos().dav.clone();
    dav.ensure_defaults(r.alice_id).await.unwrap();
    let book = dav.collection(r.alice_id, "addressbook", "default").await.unwrap().unwrap().id;
    for (name, uid, fnm) in [("ada.vcf", "ada", "Ada Lovelace"), ("alan.vcf", "alan", "Alan Turing")] {
        let body = format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:{fnm}\r\nEND:VCARD\r\n");
        let new = rusty_store::NewDavObject { name: name.into(), uid: uid.into(), body, component: "VCARD".into(), starts_at: None, ends_at: None, recurring: false };
        dav.put(book, &new, &rusty_store::DavPrecondition::None).await.unwrap();
    }
    let s = alice(&r).await;
    let out = call(r.addr, "GET", "/api/v1/contacts/export", &[("Cookie", &s.cookie)], None).await;
    assert_eq!(out.status, 200, "{}", out.text());
    let text = out.text();
    assert_eq!(text.matches("BEGIN:VCARD").count(), 2, "{text}");
    assert!(text.contains("FN:Ada Lovelace") && text.contains("FN:Alan Turing") && text.ends_with("END:VCARD\r\n"), "{text}");
    assert!(out.header("content-disposition").is_some_and(|d| d.contains("attachment") && d.contains("contacts.vcf")));
    assert!(out.header("content-type").is_some_and(|t| t.starts_with("text/vcard")));
    // No session, no file; another person gets an empty file.
    assert_eq!(call(r.addr, "GET", "/api/v1/contacts/export", &[], None).await.status, 401);
    let (bob_cookie, _) = sign_in(r.addr, "bob@one.test", "another-long-password-2").await;
    let none = call(r.addr, "GET", "/api/v1/contacts/export", &[("Cookie", &bob_cookie)], None).await;
    assert_eq!(none.status, 200);
    assert_eq!(none.text(), "");
}

#[tokio::test]
async fn the_agenda_lists_the_events_of_the_person_in_a_window_with_the_repeating_ones_expanded() {
    let r = rig().await;
    let dav = r.operator.repos().dav.clone();
    dav.ensure_defaults(r.alice_id).await.unwrap();
    let cal = dav.collection(r.alice_id, "calendar", "default").await.unwrap().unwrap().id;
    // Bounds as the DAV service keeps them: the one-off is bounded, the daily rule has no end.
    let one = rusty_dav::ical::read("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//t//EN\r\nBEGIN:VEVENT\r\nUID:one\r\nDTSTAMP:20240101T000000Z\r\nDTSTART:20240305T100000Z\r\nDTEND:20240305T110000Z\r\nSUMMARY:Dentist\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n").unwrap();
    let daily = rusty_dav::ical::read("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//t//EN\r\nBEGIN:VEVENT\r\nUID:daily\r\nDTSTAMP:20240101T000000Z\r\nDTSTART:20240301T080000Z\r\nDTEND:20240301T083000Z\r\nRRULE:FREQ=DAILY\r\nSUMMARY:Stand-up\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n").unwrap();
    for (name, o, lines) in [
        ("one.ics", &one, "DTSTART:20240305T100000Z\r\nDTEND:20240305T110000Z\r\nSUMMARY:Dentist\r\n"),
        ("daily.ics", &daily, "DTSTART:20240301T080000Z\r\nDTEND:20240301T083000Z\r\nRRULE:FREQ=DAILY\r\nSUMMARY:Stand-up\r\n"),
    ] {
        let body = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//t//EN\r\nBEGIN:VEVENT\r\nUID:{}\r\nDTSTAMP:20240101T000000Z\r\n{lines}END:VEVENT\r\nEND:VCALENDAR\r\n", o.uid);
        let new = rusty_store::NewDavObject { name: name.into(), uid: o.uid.clone(), body, component: "VEVENT".into(), starts_at: o.starts_at, ends_at: o.ends_at, recurring: o.recurring };
        dav.put(cal, &new, &rusty_store::DavPrecondition::None).await.unwrap();
    }
    let s = alice(&r).await;
    let ask = |q: String| {
        let (addr, cookie) = (r.addr, s.cookie.clone());
        async move { call(addr, "GET", &format!("/api/v1/events?{q}"), &[("Cookie", &cookie)], None).await }
    };
    // 2024-03-04 .. 2024-03-07 (UTC): three stand-ups (4th, 5th, 6th) and the dentist on the 5th.
    let (from, to) = (1_709_510_400, 1_709_769_600);
    let res = ask(format!("from={from}&to={to}")).await;
    assert_eq!(res.status, 200, "{}", res.text());
    let items = res.json()["items"].as_array().unwrap().clone();
    let names: Vec<&str> = items.iter().map(|i| i["summary"].as_str().unwrap()).collect();
    assert_eq!(names, ["Stand-up", "Stand-up", "Dentist", "Stand-up"], "{items:?}");
    assert!(items.windows(2).all(|w| w[0]["start"].as_i64() <= w[1]["start"].as_i64()), "sorted by start");
    assert_eq!(items[2]["utc"], true);
    assert_eq!(res.json()["truncated"], false);
    // A window outside everything the one-off touches has only the daily rule; a bad window is refused; no session, no agenda.
    assert_eq!(ask("from=5&to=1".into()).await.status, 400);
    assert_eq!(ask(format!("from={from}")).await.status, 400);
    assert_eq!(call(r.addr, "GET", &format!("/api/v1/events?from={from}&to={to}"), &[], None).await.status, 401);
    // Another person sees none of it.
    let (bob_cookie, _) = sign_in(r.addr, "bob@one.test", "another-long-password-2").await;
    let none = call(r.addr, "GET", &format!("/api/v1/events?from={from}&to={to}"), &[("Cookie", &bob_cookie)], None).await;
    assert_eq!((none.status, none.json()["items"].as_array().unwrap().len()), (200, 0));
}

#[tokio::test]
async fn a_vcf_file_is_imported_into_the_address_book_and_is_what_the_composer_and_the_export_see() {
    let r = rig().await;
    let dav = r.operator.repos().dav.clone();
    dav.ensure_defaults(r.alice_id).await.unwrap();
    let s = alice(&r).await;
    let vcf = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:ada-1\r\nFN:Ada Lovelace\r\nEMAIL:ada@example.org\r\nEND:VCARD\r\nBEGIN:VCARD\r\nVERSION:3.0\r\nFN:Grace Hopper\r\nEMAIL:grace@navy.example\r\nEND:VCARD\r\nBEGIN:VCARD\r\nVERSION:3.0\r\nEND:VCARD\r\n";
    let first = s.post(r.addr, "/api/v1/contacts/import", serde_json::json!({ "vcf": vcf })).await;
    assert_eq!(first.status, 200, "{}", first.text());
    assert_eq!(first.json(), serde_json::json!({ "imported": 2, "updated": 0, "invalid": 1, "duplicates": 0 }));
    // Seen at once by the composer and the export.
    let found = call(r.addr, "GET", "/api/v1/contacts?q=grace", &[("Cookie", &s.cookie)], None).await;
    assert_eq!(found.json()["items"], serde_json::json!([{ "name": "Grace Hopper", "email": "grace@navy.example" }]));
    let out = call(r.addr, "GET", "/api/v1/contacts/export", &[("Cookie", &s.cookie)], None).await.text();
    assert_eq!(out.matches("BEGIN:VCARD").count(), 2, "{out}");
    // Importing the file again changes nothing but the card that has a UID (the other one had none, so it is a new card each time).
    let again = s.post(r.addr, "/api/v1/contacts/import", serde_json::json!({ "vcf": vcf })).await;
    assert_eq!(again.json()["updated"], 1, "{}", again.text());
    // A CardDAV client sees it as a collection change (the sync token moved).
    let book = dav.collection(r.alice_id, "addressbook", "default").await.unwrap().unwrap();
    assert!(book.revision >= 3, "{}", book.revision);
    // No session, no CSRF token, no cards and no book are all refused.
    assert_eq!(call(r.addr, "POST", "/api/v1/contacts/import", &[("Content-Type", "application/json"), ("Origin", ORIGIN)], Some("{\"vcf\":\"x\"}")).await.status, 401);
    let no_csrf = call(r.addr, "POST", "/api/v1/contacts/import", &[("Cookie", &s.cookie), ("Content-Type", "application/json"), ("Origin", ORIGIN)], Some("{\"vcf\":\"x\"}")).await;
    assert!(matches!(no_csrf.status, 401 | 403), "{}", no_csrf.status);
    assert_eq!(s.post(r.addr, "/api/v1/contacts/import", serde_json::json!({ "vcf": "nothing here" })).await.status, 400);
    assert_eq!(s.post(r.addr, "/api/v1/contacts/import", serde_json::json!({ "nope": 1 })).await.status, 400);
    let (bob_cookie, bob_csrf) = sign_in(r.addr, "bob@one.test", "another-long-password-2").await;
    let nobook = call(r.addr, "POST", "/api/v1/contacts/import", &[("Cookie", &bob_cookie), ("Content-Type", "application/json"), ("Origin", ORIGIN), ("X-CSRF-Token", &bob_csrf)], Some(&serde_json::json!({ "vcf": vcf }).to_string())).await;
    assert_eq!(nobook.status, 409, "bob has no address book: the DAV service makes it, not the webmail");
}

#[tokio::test]
async fn a_new_event_is_added_to_the_default_calendar_and_is_what_the_agenda_lists() {
    let r = rig().await;
    let dav = r.operator.repos().dav.clone();
    dav.ensure_defaults(r.alice_id).await.unwrap();
    let s = alice(&r).await;
    let (start, end) = (1_709_634_000_i64, 1_709_637_600_i64);
    let made = s.post(r.addr, "/api/v1/events", serde_json::json!({ "summary": "Lunch", "location": "Cafe", "start": start, "end": end })).await;
    assert_eq!(made.status, 201, "{}", made.text());
    assert!(made.json()["uid"].as_str().is_some_and(|u| u.starts_with("urn:uuid:")));
    let listed = call(r.addr, "GET", &format!("/api/v1/events?from={}&to={}", start - 3600, end + 3600), &[("Cookie", &s.cookie)], None).await;
    let items = listed.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!((items[0]["summary"].as_str(), items[0]["location"].as_str(), items[0]["start"].as_i64(), items[0]["utc"].as_bool()), (Some("Lunch"), Some("Cafe"), Some(start), Some(true)));
    // The calendar's change count moved, so a CalDAV client syncs it.
    let cal = dav.collection(r.alice_id, "calendar", "default").await.unwrap().unwrap();
    assert!(cal.revision >= 1, "{}", cal.revision);
    // Refused: no title, an end before the start, no session or token; and a person with no calendar gets a 409.
    assert_eq!(s.post(r.addr, "/api/v1/events", serde_json::json!({ "summary": "", "start": start, "end": end })).await.status, 400);
    assert_eq!(s.post(r.addr, "/api/v1/events", serde_json::json!({ "summary": "x", "start": end, "end": start })).await.status, 400);
    assert_eq!(call(r.addr, "POST", "/api/v1/events", &[("Content-Type", "application/json"), ("Origin", ORIGIN)], Some("{}")).await.status, 401);
    let (bob_cookie, bob_csrf) = sign_in(r.addr, "bob@one.test", "another-long-password-2").await;
    let body = serde_json::json!({ "summary": "x", "start": start, "end": end }).to_string();
    let nocal = call(r.addr, "POST", "/api/v1/events", &[("Cookie", &bob_cookie), ("Content-Type", "application/json"), ("Origin", ORIGIN), ("X-CSRF-Token", &bob_csrf)], Some(&body)).await;
    assert_eq!(nocal.status, 409, "the DAV service makes the calendar, not the webmail");
}
