//! The move of a mailbox from another IMAP server in the admin web (docs/design-imap-migration.md): platform administrators only, validated, the password sealed and never shown, one at a time per
//! account, stoppable, and audited with the password hidden.

mod common;

use common::*;
use serde_json::json;

fn ask() -> serde_json::Value {
    json!({ "host": "imap.old.example", "port": 993, "security": "tls", "user": "alice@old.example", "password": "S3cret-pass-word" })
}

#[tokio::test]
async fn a_move_is_asked_for_by_a_platform_administrator_sealed_stoppable_and_audited_without_the_password() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "tenant@one.test", true).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(root.post("/api/v1/domains/one.test/accounts", json!({ "local": "alice", "password": "secret-password-9" })).await.status, 201);

    // Only a platform administrator.
    let tenant = Client::sign_in(r.addr, "tenant@one.test").await;
    assert_eq!(tenant.mutate("POST", "/api/v1/accounts/alice@one.test/migrations", Some(ask())).await.status, 403);
    assert_eq!(tenant.get("/api/v1/accounts/alice@one.test/migrations").await.status, 403);

    // What is asked has to be sensible.
    for bad in [
        json!({ "host": "bad host", "port": 993, "security": "tls", "user": "a", "password": "x" }),
        json!({ "host": "h.example", "port": 0, "security": "tls", "user": "a", "password": "x" }),
        json!({ "host": "h.example", "port": 993, "security": "ssl", "user": "a", "password": "x" }),
        json!({ "host": "h.example", "port": 993, "security": "tls", "user": " ", "password": "x" }),
        json!({ "host": "h.example", "port": 993, "security": "tls", "user": "a", "password": "" }),
        json!({ "host": "h.example", "port": 993, "security": "tls", "user": "a", "password": "line\nbreak" }),
        json!({ "host": "h.example", "port": 993, "security": "tls", "user": "a", "password": "x", "extra": 1 }),
    ] {
        assert_eq!(root.mutate("POST", "/api/v1/accounts/alice@one.test/migrations", Some(bad.clone())).await.status, 400, "{bad}");
    }
    assert_eq!(root.mutate("POST", "/api/v1/accounts/nobody@one.test/migrations", Some(ask())).await.status, 404);

    let made = root.mutate("POST", "/api/v1/accounts/alice@one.test/migrations", Some(ask())).await;
    assert_eq!(made.status, 201, "{}", made.body);
    conforms(&made.json(), "Migration");
    assert_eq!((made.json()["state"].clone(), made.json()["host"].clone(), made.json()["user"].clone()), (json!("queued"), json!("imap.old.example"), json!("alice@old.example")));
    assert!(!made.body.contains("S3cret-pass-word"), "the password is never shown");
    let id = made.json()["id"].as_i64().unwrap();

    // The password is a sealed secret of this migration, and the plain text is nowhere in the database.
    assert_eq!(r.fx.scalar("SELECT count(*) FROM sealed_secrets WHERE owner_kind = 'migration' AND owner_id = ? AND purpose = 'password'", &[rusty_store::fixtures::Arg::I(id)]).await, 1);
    assert_eq!(r.fx.scalar("SELECT count(*) FROM sealed_secrets WHERE position('S3cret' in encode(sealed, 'escape')) > 0", &[]).await, 0);

    // One at a time.
    assert_eq!(root.mutate("POST", "/api/v1/accounts/alice@one.test/migrations", Some(ask())).await.status, 409);
    let list = root.get("/api/v1/accounts/alice@one.test/migrations").await;
    assert_eq!(list.status, 200, "{}", list.body);
    conforms(&list.json(), "MigrationList");
    assert_eq!(list.json()["items"].as_array().unwrap().len(), 1);

    // Stopped while waiting: over at once, and then there is nothing to stop.
    let stopped = root.mutate("DELETE", &format!("/api/v1/accounts/alice@one.test/migrations/{id}"), None).await;
    assert_eq!(stopped.status, 200, "{}", stopped.body);
    assert_eq!(stopped.json()["state"], json!("cancelled"));
    assert_eq!(root.mutate("DELETE", &format!("/api/v1/accounts/alice@one.test/migrations/{id}"), None).await.status, 409);
    assert_eq!(root.mutate("DELETE", "/api/v1/accounts/alice@one.test/migrations/999999", None).await.status, 404);
    // Another one may be asked for now.
    assert_eq!(root.mutate("POST", "/api/v1/accounts/alice@one.test/migrations", Some(ask())).await.status, 201);

    // The audit log has what was asked for and not the password.
    let audit = root.get("/api/v1/audit").await;
    assert!(!audit.body.contains("S3cret-pass-word"), "{}", audit.body);
    let entry = audit.json()["items"].as_array().unwrap().iter().find(|i| i["path"] == json!("/api/v1/accounts/alice@one.test/migrations") && i["method"] == json!("POST")).expect("the move was audited").clone();
    let detail = entry["detail"].as_str().unwrap();
    assert!(detail.contains("imap.old.example") && detail.contains("(hidden)"), "{detail}");

    // When the rows are gone after their two days the admin service removes the sealed passwords that were left (no service that works on the mail may write a sealed secret).
    r.fx.run("DELETE FROM imap_migrations", &[]).await;
    assert_eq!(r.store.repos().migrations.purge_orphan_secrets().await.unwrap(), 2);
    assert_eq!(r.fx.scalar("SELECT count(*) FROM sealed_secrets WHERE owner_kind = 'migration'", &[]).await, 0);
}
