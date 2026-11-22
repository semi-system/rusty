//! Export and import of a mailbox in the admin web (docs/design-mailbox-transfers.md): platform administrators only, the file passes through the database in pieces, the download is in the audit log
//! and the pieces of an upload are not.

mod common;

use common::*;
use serde_json::json;

/// The body of a response that came in chunks (the raw reader of the tests does not undo them).
fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, after)) = rest.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.push_str(&after[..n]);
        rest = after[n..].trim_start_matches("\r\n");
    }
    out
}


async fn setup() -> (Rig, Client, i64) {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "tenant@one.test", true).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(c.post("/api/v1/domains/one.test/accounts", json!({ "local": "alice", "password": "alice-password-1" })).await.status, 201);
    let alice = r.dir.find_account("alice@one.test").await.unwrap().unwrap().id.0;
    (r, c, alice)
}

#[tokio::test]
async fn an_export_is_asked_for_by_a_platform_administrator_and_its_file_is_streamed_and_audited() {
    let (r, c, alice) = setup().await;
    let asked = c.post_empty("/api/v1/accounts/alice@one.test/exports").await;
    assert_eq!(asked.status, 201, "{}", asked.body);
    conforms(&asked.json(), "Transfer");
    assert_eq!((asked.json()["kind"].clone(), asked.json()["state"].clone(), asked.json()["requested_by"].clone()), (json!("export"), json!("queued"), json!("root@home.test")));
    let id = asked.json()["id"].as_i64().unwrap();
    // Not finished: nothing to download yet.
    assert_eq!(c.get(&format!("/api/v1/transfers/{id}/download")).await.status, 409);
    // The service that holds the mailboxes does the work: here a stand-in takes the job, writes two pieces and finishes it.
    let repo = r.outbound.repos().transfers.clone();
    let taken = repo.claim().await.unwrap().expect("queued");
    assert_eq!((taken.id, taken.account), (id, alice));
    assert!(repo.add_chunk(id, 0, b"From MAILER-DAEMON x\nSubject: a\n\n", 1 << 20).await.unwrap());
    assert!(repo.add_chunk(id, 1, b"body\n\n", 1 << 20).await.unwrap());
    repo.finish(id, "done", 40, 1, 0, None).await.unwrap();
    let got = c.get(&format!("/api/v1/transfers/{id}/download")).await;
    assert_eq!(got.status, 200);
    assert_eq!(dechunk(&got.body), "From MAILER-DAEMON x\nSubject: a\n\nbody\n\n");
    assert!(got.header("content-disposition").is_some_and(|h| h.starts_with("attachment; filename=\"mailbox-")), "{:?}", got.header("content-disposition"));
    assert_eq!(got.header("cache-control").as_deref(), Some("no-store"));
    let listed = c.get("/api/v1/accounts/alice@one.test/transfers").await;
    conforms(&listed.json(), "TransferList");
    assert_eq!(listed.json()["items"][0]["state"], json!("done"));
    assert_eq!(c.get(&format!("/api/v1/transfers/{id}")).await.json()["messages"], json!(1));
    // Who took the mail is in the audit log, the request and the download.
    let audit = c.get("/api/v1/audit").await.json()["items"].as_array().unwrap().iter().map(|i| i["path"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    assert!(audit.contains(&"/api/v1/accounts/alice@one.test/exports".to_owned()) && audit.contains(&format!("/api/v1/transfers/{id}/download")), "{audit:?}");
    // A domain administrator may do none of it, whatever the account.
    let t = Client::sign_in(r.addr, "tenant@one.test").await;
    assert_eq!(t.post_empty("/api/v1/accounts/alice@one.test/exports").await.status, 403);
    assert_eq!(t.get(&format!("/api/v1/transfers/{id}/download")).await.status, 403);
    assert_eq!(t.get("/api/v1/accounts/alice@one.test/transfers").await.status, 403);
    assert_eq!(c.get("/api/v1/transfers/999999/download").await.status, 404);
}

#[tokio::test]
async fn an_import_is_uploaded_in_small_pieces_and_started_only_when_every_piece_is_there() {
    let (r, c, _) = setup().await;
    let begun = c.post("/api/v1/accounts/alice@one.test/imports", json!({ "folder": "Archive" })).await;
    assert_eq!(begun.status, 201, "{}", begun.body);
    conforms(&begun.json(), "Transfer");
    assert_eq!((begun.json()["state"].clone(), begun.json()["folder"].clone()), (json!("uploading"), json!("Archive")));
    let id = begun.json()["id"].as_i64().unwrap();
    let piece = |n: u32| format!("/api/v1/transfers/{id}/chunks/{n}");
    assert_eq!(c.put_raw(&piece(0), "From a Thu Jan  1 00:00:00 1970\nSubject: x\n\nhello\n").await.status, 200);
    assert_eq!(c.put_raw(&piece(0), "again").await.status, 409, "a piece twice");
    assert_eq!(c.put_raw(&piece(1), &"x".repeat(49 * 1024)).await.status, 400, "a piece over 48 KiB");
    assert_eq!(c.put_raw(&piece(1), "").await.status, 400);
    // Starting with a piece missing, or with more pieces than it says, is refused; nothing is queued.
    assert_eq!(c.post(&format!("/api/v1/transfers/{id}/start"), json!({ "pieces": 2 })).await.status, 409);
    assert_eq!(c.put_raw(&piece(1), "\n").await.status, 200);
    assert_eq!(c.post(&format!("/api/v1/transfers/{id}/start"), json!({ "pieces": 1 })).await.status, 409, "a piece beyond the count");
    assert_eq!(c.post(&format!("/api/v1/transfers/{id}/start"), json!({ "pieces": 0 })).await.status, 400);
    let started = c.post(&format!("/api/v1/transfers/{id}/start"), json!({ "pieces": 2 })).await;
    assert_eq!(started.status, 200, "{}", started.body);
    assert_eq!(started.json()["state"], json!("queued"));
    // After that the file takes no more pieces and cannot be started twice.
    assert_eq!(c.put_raw(&piece(2), "late").await.status, 409);
    assert_eq!(c.post(&format!("/api/v1/transfers/{id}/start"), json!({ "pieces": 2 })).await.status, 409);
    // What the worker finds is what was uploaded.
    let repo = r.outbound.repos().transfers.clone();
    let taken = repo.claim().await.unwrap().unwrap();
    assert_eq!((taken.kind.as_str(), taken.folder.as_deref()), ("import", Some("Archive")));
    assert_eq!(repo.chunk(id, 0).await.unwrap().unwrap(), b"From a Thu Jan  1 00:00:00 1970\nSubject: x\n\nhello\n".to_vec());
    // The audit log has the import and its start, and not a row for each piece.
    let audit = c.get("/api/v1/audit").await.json()["items"].as_array().unwrap().iter().map(|i| i["path"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    assert!(audit.iter().any(|p| p.ends_with("/imports")) && audit.iter().any(|p| p.ends_with("/start")), "{audit:?}");
    assert!(!audit.iter().any(|p| p.contains("/chunks/")), "{audit:?}");
    assert_eq!(c.post("/api/v1/accounts/alice@one.test/imports", json!({ "folder": "bad\nname" })).await.status, 400);
    assert_eq!(c.post("/api/v1/accounts/nobody@one.test/imports", json!({})).await.status, 404);
}
