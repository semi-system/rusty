//! Who may use the admin web: the list, the grant and the revocation, with their contract shapes, tenant isolation, and the protections that keep
//! the console from being left without a platform administrator.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn a_platform_administrator_lists_grants_and_revokes_and_the_responses_have_their_contract_shapes() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    c.post("/api/v1/domains/one.test/accounts", json!({ "local": "carol", "password": "carol-password-1" })).await;
    c.post("/api/v1/domains/one.test/accounts", json!({ "local": "dave", "password": "dave-password-12" })).await;
    c.post("/api/v1/domains/one.test/accounts", json!({ "local": "nopw" })).await;

    let list = c.get("/api/v1/admins").await;
    assert_eq!(list.status, 200);
    conforms(&list.json(), "AdminList");
    assert_eq!(list.json()["items"].as_array().unwrap().len(), 1);
    assert_eq!(list.json()["items"][0]["scope"], "platform");
    assert_eq!(list.json()["items"][0]["domain"], serde_json::Value::Null);

    let g = c.post("/api/v1/admins", json!({ "address": "Carol@one.test", "domain": "one.test" })).await;
    assert_eq!(g.status, 201, "{}", g.body);
    conforms(&g.json(), "AdminEntry");
    assert_eq!(g.json()["address"], "carol@one.test");
    assert_eq!(g.json()["scope"], "domain");
    assert_eq!(g.json()["domain"], "one.test");
    assert_eq!(c.post("/api/v1/admins", json!({ "address": "dave@one.test", "domain": null })).await.status, 201, "a platform grant for an account of any domain");

    assert_eq!(c.post("/api/v1/admins", json!({ "address": "carol@one.test", "domain": null })).await.status, 409, "already an administrator");
    assert_eq!(c.post("/api/v1/admins", json!({ "address": "nopw@one.test", "domain": null })).await.status, 400, "no password, cannot sign in");
    assert_eq!(c.post("/api/v1/admins", json!({ "address": "ghost@one.test", "domain": null })).await.status, 404);
    assert_eq!(c.post("/api/v1/admins", json!({ "address": "root@home.test", "domain": "one.test" })).await.status, 409);
    assert_eq!(c.post("/api/v1/admins", json!({ "address": "alice@one.test", "extra": 1 })).await.status, 400, "unknown fields");

    let all = c.get("/api/v1/admins").await.json();
    assert_eq!(all["items"].as_array().unwrap().len(), 3);

    let gone = c.mutate("DELETE", "/api/v1/admins/carol@one.test", None).await;
    assert_eq!(gone.status, 200);
    conforms(&gone.json(), "Empty");
    assert_eq!(c.mutate("DELETE", "/api/v1/admins/carol@one.test", None).await.status, 404, "no longer an administrator");
    assert_eq!(c.get("/api/v1/admins").await.json()["items"].as_array().unwrap().len(), 2);
    // The account stays.
    assert!(r.dir.find_account("carol@one.test").await.unwrap().is_some());
}

#[tokio::test]
async fn nobody_revokes_themselves_and_the_last_platform_administrator_stays() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "second@home.test", false).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    let second = Client::sign_in(r.addr, "second@home.test").await;
    assert_eq!(root.mutate("DELETE", "/api/v1/admins/root@home.test", None).await.status, 400, "not oneself");
    assert_eq!(root.mutate("DELETE", "/api/v1/admins/second@home.test", None).await.status, 200);
    assert_eq!(second.get("/api/v1/session").await.status, 401, "the revoked administrator is out at its next request");
    // Only one platform administrator is left: another one cannot take it away either.
    make_admin(&r, "boss@home.test", true).await;
    let boss = Client::sign_in(r.addr, "boss@home.test").await;
    assert_eq!(boss.mutate("DELETE", "/api/v1/admins/root@home.test", None).await.status, 403, "a domain administrator does not touch grants");
    assert_eq!(r.admins.revoke_keeping_one(r.dir.find_account("root@home.test").await.unwrap().unwrap().id).await.unwrap(), rusty_store::Revoked::LastPlatform);
    assert_eq!(root.get("/api/v1/session").await.status, 200);
}

#[tokio::test]
async fn a_domain_administrator_sees_only_its_own_domains_administrators_and_cannot_change_grants() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "boss@one.test", true).await;
    make_admin(&r, "other@two.test", true).await;
    let boss = Client::sign_in(r.addr, "boss@one.test").await;
    let l = boss.get("/api/v1/admins").await;
    assert_eq!(l.status, 200);
    let items = l.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["address"], "boss@one.test");
    for (m, p, b) in [
        ("POST", "/api/v1/admins", Some(json!({ "address": "boss@one.test", "domain": null }))),
        ("DELETE", "/api/v1/admins/other@two.test", None),
        ("DELETE", "/api/v1/admins/root@home.test", None),
    ] {
        assert_eq!(boss.mutate(m, p, b).await.status, 403, "{m} {p}");
    }
    assert!(r.admins.get(r.dir.find_account("other@two.test").await.unwrap().unwrap().id).await.unwrap().is_some());
}

#[tokio::test]
async fn the_list_needs_a_session_and_refuses_a_malformed_cursor() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    assert_eq!(send(r.addr, "GET", "/api/v1/admins", &[], "").await.status, 401);
    let c = Client::sign_in(r.addr, "root@home.test").await;
    for q in ["after=-1", "after=x", "after=%2B1", "x=1"] {
        assert_eq!(c.get(&format!("/api/v1/admins?{q}")).await.status, 400, "{q}");
    }
    assert_eq!(c.get("/api/v1/admins?after=0").await.status, 200);
}

#[tokio::test]
async fn a_helpdesk_administrator_may_list_accounts_and_reset_passwords_and_nothing_more() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "help@one.test", false).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(root.post("/api/v1/domains/one.test/accounts", json!({ "local": "alice", "password": "alice-password-1" })).await.status, 201);
    // A helpdesk grant needs a domain; revoking and granting again with the role.
    assert_eq!(root.mutate("DELETE", "/api/v1/admins/help@one.test", None).await.status, 200);
    assert_eq!(root.post("/api/v1/admins", json!({ "address": "help@one.test", "domain": null, "role": "helpdesk" })).await.status, 400);
    assert_eq!(root.post("/api/v1/admins", json!({ "address": "help@one.test", "domain": "one.test", "role": "bogus" })).await.status, 400);
    let made = root.post("/api/v1/admins", json!({ "address": "help@one.test", "domain": "one.test", "role": "helpdesk" })).await;
    assert_eq!(made.status, 201, "{}", made.body);
    conforms(&made.json(), "AdminEntry");
    assert_eq!(made.json()["role"], json!("helpdesk"));

    let help = Client::sign_in(r.addr, "help@one.test").await;
    assert_eq!(help.get("/api/v1/domains/one.test/accounts").await.status, 200, "may look at the accounts of its domain");
    assert_eq!(help.post("/api/v1/accounts/alice@one.test/password", json!({ "password": "a-new-password-2", "must_change": true })).await.status, 200);
    assert_eq!(help.post_empty("/api/v1/accounts/alice@one.test/lock").await.status, 200);
    assert_eq!(help.post_empty("/api/v1/accounts/alice@one.test/enable").await.status, 200);
    // Nothing else: no new account, no deletion, no administrators, no settings, no audit log.
    assert_eq!(help.post("/api/v1/domains/one.test/accounts", json!({ "local": "bob", "password": "bob-password-12" })).await.status, 403);
    assert_eq!(help.mutate("DELETE", "/api/v1/accounts/alice@one.test", None).await.status, 403);
    assert_eq!(help.get("/api/v1/admins").await.status, 403);
    assert_eq!(help.get("/api/v1/audit").await.status, 403);
    assert_eq!(help.get("/api/v1/domains/one.test/settings").await.status, 403);
}
