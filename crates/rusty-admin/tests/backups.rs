//! The Backups page's answer (docs/design-backup-status.md): what the tool recorded, newest first, for platform administrators only.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn the_history_is_newest_first_and_only_a_platform_administrator_sees_it() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "tenant@one.test", true).await;
    // The tool writes with its own login (here: the owner of the schema); the admin web's role only reads.
    for sql in [
        "INSERT INTO backup_runs (kind, ok, bytes, detail) VALUES ('backup', true, 186894, '')",
        "INSERT INTO backup_runs (kind, ok, bytes, detail) VALUES ('verify', true, NULL, 'migrations=39 newest=47 domains=2 accounts=2 messages=3')",
        "INSERT INTO backup_runs (kind, ok, bytes, detail) VALUES ('verify', false, NULL, 'pg_restore reported errors')",
    ] {
        r.fx.run(sql, &[]).await;
    }

    let root = Client::sign_in(r.addr, "root@home.test").await;
    let got = root.get("/api/v1/backups").await;
    assert_eq!(got.status, 200, "{}", got.body);
    let body = got.json();
    assert!(body["now"].as_i64().unwrap() > 1_700_000_000);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!((items[0]["kind"].clone(), items[0]["ok"].clone(), items[0]["bytes"].clone()), (json!("verify"), json!(false), json!(null)), "newest first");
    assert_eq!(items[1]["detail"], json!("migrations=39 newest=47 domains=2 accounts=2 messages=3"));
    assert_eq!((items[2]["kind"].clone(), items[2]["bytes"].clone()), (json!("backup"), json!(186_894)));

    let tenant = Client::sign_in(r.addr, "tenant@one.test").await;
    assert_eq!(tenant.get("/api/v1/backups").await.status, 403, "a domain administrator does not see the platform's backups");
}
