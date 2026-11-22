//! The audit log (docs/design-audit.md): a change is written with who and what, a read and a refusal are not, what was set is kept as a summary with the secrets hidden, and a domain administrator sees only its own domain.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn changes_are_recorded_with_their_actor_and_a_domain_administrator_sees_its_own_domain_only() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "tenant@one.test", true).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(root.post("/api/v1/domains", json!({ "name": "two.test" })).await.status, 201);
    assert_eq!(root.post("/api/v1/domains/one.test/accounts", json!({ "local": "alice", "password": "secret-password-9" })).await.status, 201);
    assert_eq!(root.post("/api/v1/domains/two.test/accounts", json!({ "local": "bob", "password": "secret-password-9" })).await.status, 201);
    // A refused change (a duplicate) and a read are not recorded.
    assert_eq!(root.post("/api/v1/domains/two.test/accounts", json!({ "local": "bob", "password": "secret-password-9" })).await.status, 409);
    let _ = root.get("/api/v1/domains/one.test/accounts").await;

    let all = root.get("/api/v1/audit").await;
    assert_eq!(all.status, 200, "{}", all.body);
    let items = all.json()["items"].as_array().unwrap().clone();
    let paths: Vec<&str> = items.iter().map(|i| i["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["/api/v1/domains/two.test/accounts", "/api/v1/domains/one.test/accounts", "/api/v1/domains"], "newest first, only the changes that happened");
    assert_eq!(items[0]["actor"], json!("root@home.test"));
    assert_eq!(items[0]["route"], json!("/api/v1/domains/{d}/accounts"));
    assert!(!all.body.contains("secret-password"), "a password is never kept");
    let detail: serde_json::Value = serde_json::from_str(items[0]["detail"].as_str().unwrap()).unwrap();
    assert_eq!(detail, json!({ "local": "bob", "password": "(hidden)" }), "the fields are there, the secret is not");
    assert!(items[2]["detail"].as_str().unwrap().contains("two.test"), "a domain was created with this name");

    let tenant = Client::sign_in(r.addr, "tenant@one.test").await;
    let own = tenant.get("/api/v1/audit").await;
    let own_paths: Vec<String> = own.json()["items"].as_array().unwrap().iter().map(|i| i["path"].as_str().unwrap().to_owned()).collect();
    assert_eq!(own_paths, ["/api/v1/domains/one.test/accounts"], "only the changes of its domain");
    assert_eq!(tenant.get("/api/v1/audit?domain=two.test").await.status, 404, "another domain is not there for it");
    let filtered = root.get("/api/v1/audit?domain=two.test").await;
    assert_eq!(filtered.json()["items"].as_array().unwrap().len(), 1);
    assert_eq!(root.get("/api/v1/audit?bogus=1").await.status, 400);
}
