//! The management endpoints over a real listener and directory: what each administrator may touch (tenant isolation by construction), the CSRF guards on
//! every mutation, the self-lockout protections, pagination, and the contract (`contract/v1.json`): the real responses have exactly the declared shapes and
//! the service has exactly the declared routes.

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::*;
use serde_json::{json, Value};

/// The routes the service serves: every contract endpoint answers (never the router's own 404 or 405 for a known route) and a route the contract does not
/// list does not exist.
#[tokio::test]
async fn the_routes_of_the_service_are_exactly_the_ones_in_the_contract() {
    let r = rig().await;
    let c = contract();
    for e in c["endpoints"].as_array().unwrap() {
        let method = e["method"].as_str().unwrap();
        let path = e["path"].as_str().unwrap().replace("{d}", "x.test").replace("{a}", "a@x.test");
        let reply = send(r.addr, method, &path, &[], "").await;
        // Without a session every listed route answers 401 (or the public ones 200/400/403/404); none is the router's "no route" 404 with the
        // generic body, which the service never produces for a listed route: a listed route never answers 405.
        assert_ne!(reply.status, 405, "{method} {path}");
        if e["auth"] == "admin" || e["auth"] == "platform" {
            assert_eq!(reply.status, 401, "{method} {path} without a session");
        }
    }
    // Not in the contract: no route.
    for (m, p) in [("GET", "/api/v1/accounts"), ("PUT", "/api/v1/domains"), ("DELETE", "/api/v1/domains/x.test"), ("GET", "/api/v1/accounts/a@x.test/password"), ("POST", "/api/v1/admin")] {
        let reply = send(r.addr, m, p, &[], "").await;
        assert!(reply.status == 404 || reply.status == 405, "{m} {p}: {}", reply.status);
    }
}

// ------------------------------------------------------------------------------------------------ platform administrator

#[tokio::test]
async fn a_platform_administrator_manages_domains_accounts_and_aliases_and_every_response_has_its_contract_shape() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;

    // Domains.
    let created = c.post("/api/v1/domains", json!({ "name": "One.Test" })).await;
    assert_eq!(created.status, 201, "{}", created.body);
    conforms(&created.json(), "Domain");
    assert_eq!((created.json()["name"].as_str(), created.json()["status"].as_str()), (Some("one.test"), Some("active")));
    let dup = c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    assert_eq!((dup.status, dup.json()["error"].clone()), (409, "exists".into()));
    assert_eq!(c.post("/api/v1/domains", json!({ "name": "not a domain" })).await.status, 400);
    let listed = c.get("/api/v1/domains").await;
    conforms(&listed.json(), "DomainList");
    let names: Vec<&str> = listed.json()["items"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect::<Vec<_>>().into_iter().map(|s| Box::leak(s.to_owned().into_boxed_str()) as &str).collect();
    assert_eq!(names, ["home.test", "one.test"]);

    // Accounts: the first one of a domain also gets postmaster and abuse.
    let a = c.post("/api/v1/domains/one.test/accounts", json!({ "local": "Alice", "password": "alice-password-1" })).await;
    assert_eq!(a.status, 201, "{}", a.body);
    conforms(&a.json(), "AccountCreated");
    assert_eq!(a.json()["address"], "alice@one.test");
    assert_eq!(a.json()["aliases_created"], json!(["postmaster@one.test", "abuse@one.test"]));
    let b = c.post("/api/v1/domains/one.test/accounts", json!({ "local": "bob" })).await;
    assert_eq!((b.status, b.json()["has_password"].clone(), b.json()["aliases_created"].clone()), (201, false.into(), json!([])));
    assert_eq!(c.post("/api/v1/domains/one.test/accounts", json!({ "local": "bob" })).await.status, 409);
    assert_eq!(c.post("/api/v1/domains/one.test/accounts", json!({ "local": "carol", "password": "short" })).await.status, 400);
    assert_eq!(c.post("/api/v1/domains/one.test/accounts", json!({ "local": "a@b" })).await.status, 400);
    let accounts = c.get("/api/v1/domains/one.test/accounts").await;
    conforms(&accounts.json(), "AccountList");
    assert_eq!(accounts.json()["items"].as_array().unwrap().len(), 2);
    assert!(!accounts.body.contains("hash") && !accounts.body.contains("argon"), "no secret in a response");

    // Aliases.
    let al = c.get("/api/v1/domains/one.test/aliases").await;
    conforms(&al.json(), "AliasList");
    assert_eq!(al.json()["items"].as_array().unwrap().len(), 2, "postmaster and abuse");
    let made = c.post("/api/v1/domains/one.test/aliases", json!({ "local": "info", "target": "alice@one.test" })).await;
    assert_eq!(made.status, 201, "{}", made.body);
    conforms(&made.json(), "AliasCreated");
    assert_eq!(c.post("/api/v1/domains/one.test/aliases", json!({ "local": "info", "target": "alice@one.test" })).await.status, 409);
    let gone = c.mutate("DELETE", "/api/v1/aliases/info@one.test", None).await;
    assert_eq!(gone.status, 200);
    conforms(&gone.json(), "Empty");
    assert_eq!(c.mutate("DELETE", "/api/v1/aliases/info@one.test", None).await.status, 404);

    // Account actions.
    let pw = c.post("/api/v1/accounts/bob@one.test/password", json!({ "password": "bob-password-1" })).await;
    assert_eq!(pw.status, 200, "{}", pw.body);
    conforms(&pw.json(), "Empty");
    assert!(r.dir.find_account("bob@one.test").await.unwrap().unwrap().has_password);
    assert_eq!(c.post("/api/v1/accounts/bob@one.test/password", json!({ "password": "short" })).await.status, 400);
    let off = c.post_empty("/api/v1/accounts/bob@one.test/disable").await;
    assert_eq!(off.status, 200, "{}", off.body);
    conforms(&off.json(), "Account");
    assert_eq!(off.json()["status"], "disabled");
    assert_eq!(c.post_empty("/api/v1/accounts/bob@one.test/enable").await.json()["status"], "active");

    // Domain status.
    let d = c.post_empty("/api/v1/domains/one.test/disable").await;
    assert_eq!((d.status, d.json()["status"].clone()), (200, "disabled".into()));
    conforms(&d.json(), "Domain");
    assert_eq!(c.post_empty("/api/v1/domains/one.test/enable").await.json()["status"], "active");
}

#[tokio::test]
async fn lists_are_paged_by_cursor() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let home = r.dir.find_domain("home.test").await.unwrap().unwrap().id;
    for i in 0..104 {
        r.dir.create_account(&format!("user{i:03}"), home, None, None).await.unwrap();
    }
    let c = Client::sign_in(r.addr, "root@home.test").await;
    let first = c.get("/api/v1/domains/home.test/accounts").await.json();
    assert_eq!(first["items"].as_array().unwrap().len(), 100);
    let next = first["next"].as_i64().expect("a full page has a cursor");
    let second = c.get(&format!("/api/v1/domains/home.test/accounts?after={next}")).await.json();
    assert_eq!(second["items"].as_array().unwrap().len(), 5, "105 accounts in all: the administrator and 104 more");
    assert!(second["next"].is_null(), "the last page has no cursor");
    let ids: BTreeSet<i64> = first["items"].as_array().unwrap().iter().chain(second["items"].as_array().unwrap()).map(|a| a["id"].as_i64().unwrap()).collect();
    assert_eq!(ids.len(), 105, "no row twice, none missed");
    for bad in ["?after=x", "?after=-1", "?after=+5", "?after=", "?limit=5", "?after=1&after=2"] {
        assert_eq!(c.get(&format!("/api/v1/domains/home.test/accounts{bad}")).await.status, 400, "{bad}");
    }
}

// ------------------------------------------------------------------------------------------------ tenant isolation

#[tokio::test]
async fn a_domain_administrator_cannot_see_or_touch_another_tenant_and_cannot_tell_it_exists() {
    let r = rig().await;
    make_admin(&r, "boss@one.test", true).await;
    make_admin(&r, "boss@two.test", true).await;
    let two = r.dir.find_domain("two.test").await.unwrap().unwrap().id;
    let victim = r.dir.create_account("victim", two, Some(PW), None).await.unwrap();
    r.dir.add_alias("keep", two, victim).await.unwrap();
    let one = r.dir.find_domain("one.test").await.unwrap().unwrap().id;
    r.dir.create_account("mine", one, Some(PW), None).await.unwrap();
    let c = Client::sign_in(r.addr, "boss@one.test").await;

    // The list shows its own domain only.
    let listed = c.get("/api/v1/domains").await.json();
    assert_eq!(listed["items"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect::<Vec<_>>(), ["one.test"]);

    // Every request about another tenant is 404, byte for byte the answer for a tenant that does not exist.
    type Case = (&'static str, String, Option<Value>, String, Option<Value>);
    let pairs: Vec<Case> = vec![
        ("GET", "/api/v1/domains/two.test/accounts".into(), None, "/api/v1/domains/nowhere.test/accounts".into(), None),
        ("GET", "/api/v1/domains/two.test/aliases".into(), None, "/api/v1/domains/nowhere.test/aliases".into(), None),
        ("POST", "/api/v1/domains/two.test/accounts".into(), Some(json!({ "local": "x" })), "/api/v1/domains/nowhere.test/accounts".into(), Some(json!({ "local": "x" }))),
        ("POST", "/api/v1/domains/two.test/aliases".into(), Some(json!({ "local": "x", "target": "victim@two.test" })), "/api/v1/domains/nowhere.test/aliases".into(), Some(json!({ "local": "x", "target": "victim@two.test" }))),
        ("POST", "/api/v1/accounts/victim@two.test/password".into(), Some(json!({ "password": "hijacked-pw-1" })), "/api/v1/accounts/nobody@nowhere.test/password".into(), Some(json!({ "password": "hijacked-pw-1" }))),
        ("POST", "/api/v1/accounts/victim@two.test/disable".into(), None, "/api/v1/accounts/nobody@nowhere.test/disable".into(), None),
        ("POST", "/api/v1/accounts/victim@two.test/enable".into(), None, "/api/v1/accounts/nobody@nowhere.test/enable".into(), None),
        ("DELETE", "/api/v1/aliases/keep@two.test".into(), None, "/api/v1/aliases/keep@nowhere.test".into(), None),
    ];
    for (m, foreign, fb, missing, mb) in pairs {
        let a = c.mutate(m, &foreign, fb).await;
        let b = c.mutate(m, &missing, mb).await;
        assert_eq!((a.status, a.body.as_str()), (404, "{\"error\":\"not_found\"}"), "{m} {foreign}");
        assert_eq!((a.status, &a.body), (b.status, &b.body), "{m} {foreign}: the answer must not reveal that the tenant exists");
    }
    // An alias in my own domain pointing at another tenant's account: "no such account", and nothing is created.
    let cross = c.post("/api/v1/domains/one.test/aliases", json!({ "local": "steal", "target": "victim@two.test" })).await;
    assert_eq!(cross.status, 404);
    assert!(r.dir.resolve("steal@one.test").await.is_err());
    // An account of my own domain and an unknown account of my own domain are also the same 404.
    let own_missing = c.post_empty("/api/v1/accounts/nobody@one.test/disable").await;
    assert_eq!(own_missing.status, 404);

    // Nothing of the other tenant changed.
    assert_eq!(r.dir.find_account("victim@two.test").await.unwrap().unwrap().status, rusty_directory::AccountStatus::Active);
    assert!(r.dir.resolve("keep@two.test").await.is_ok(), "the alias is still there");

    // Platform-only actions are forbidden, not hidden: the administrator knows it is a rule, not a missing thing.
    assert_eq!(c.post("/api/v1/domains", json!({ "name": "new.test" })).await.status, 403);
    assert_eq!(c.post_empty("/api/v1/domains/one.test/disable").await.status, 403);
    assert_eq!(c.post_empty("/api/v1/domains/one.test/enable").await.status, 403);
    assert!(r.dir.find_domain("new.test").await.unwrap().is_none());

    // Its own domain works.
    assert_eq!(c.post("/api/v1/domains/one.test/accounts", json!({ "local": "new1" })).await.status, 201);
    assert_eq!(c.get("/api/v1/domains/one.test/accounts").await.status, 200);
}

#[tokio::test]
async fn the_cross_tenant_matrix_holds_for_every_scoped_endpoint_with_the_other_domain_spelled_oddly() {
    let r = rig().await;
    make_admin(&r, "boss@one.test", true).await;
    make_admin(&r, "boss@two.test", true).await;
    let c = Client::sign_in(r.addr, "boss@one.test").await;
    for spelling in ["TWO.TEST", "two.test.", "Two.Test", "%74wo.test", "two.test%2F", "two.test%00", "..%2Ftwo.test"] {
        let list = c.get(&format!("/api/v1/domains/{spelling}/accounts")).await;
        assert!(list.status == 404 || list.status == 400, "{spelling}: {}", list.status);
        assert!(!list.body.contains("boss"), "{spelling}: nothing of the other tenant leaks: {}", list.body);
    }
    for spelling in ["boss@TWO.TEST", "BOSS@two.test", "boss%40two.test", "boss@two.test.", "boss@@two.test", "boss%40one.test%40two.test"] {
        let r1 = c.post_empty(&format!("/api/v1/accounts/{spelling}/disable")).await;
        assert!(r1.status == 404 || r1.status == 400, "{spelling}: {}", r1.status);
    }
    let boss2 = r.dir.find_account("boss@two.test").await.unwrap().unwrap();
    assert_eq!(boss2.status, rusty_directory::AccountStatus::Active, "the other tenant's administrator was never touched");
}

// ------------------------------------------------------------------------------------------------ guards and protections

#[tokio::test]
async fn every_mutation_needs_a_session_and_all_three_csrf_guards_and_none_changes_anything_without_them() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    c.post("/api/v1/domains/one.test/accounts", json!({ "local": "alice", "password": "alice-password-1" })).await;
    let mutations: Vec<(&str, &str, &str)> = vec![
        ("POST", "/api/v1/domains", "{\"name\":\"evil.test\"}"),
        ("POST", "/api/v1/domains/one.test/disable", ""),
        ("POST", "/api/v1/domains/one.test/accounts", "{\"local\":\"evil\"}"),
        ("POST", "/api/v1/accounts/alice@one.test/password", "{\"password\":\"hijacked-pw-1\"}"),
        ("POST", "/api/v1/accounts/alice@one.test/disable", ""),
        ("POST", "/api/v1/domains/one.test/aliases", "{\"local\":\"evil\",\"target\":\"alice@one.test\"}"),
        ("DELETE", "/api/v1/aliases/postmaster@one.test", ""),
    ];
    for (m, p, b) in mutations {
        let ok_headers = [("Cookie", c.cookie.as_str()), ("Origin", ORIGIN), ("Content-Type", "application/json"), ("X-CSRF-Token", c.csrf.as_str())];
        // No session at all.
        assert_eq!(send(r.addr, m, p, &[("Origin", ORIGIN), ("Content-Type", "application/json")], b).await.status, 401, "{m} {p}");
        // A session without the CSRF token, with a wrong one, with a foreign Origin, without Origin, and with a form content type.
        let wrong = "0".repeat(64);
        for (what, h) in [
            ("no token", vec![ok_headers[0], ok_headers[1], ok_headers[2]]),
            ("wrong token", vec![ok_headers[0], ok_headers[1], ok_headers[2], ("X-CSRF-Token", wrong.as_str())]),
            ("foreign origin", vec![ok_headers[0], ("Origin", "https://evil.example"), ok_headers[2], ok_headers[3]]),
            ("no origin", vec![ok_headers[0], ok_headers[2], ok_headers[3]]),
        ] {
            assert_eq!(send(r.addr, m, p, &h, b).await.status, 403, "{m} {p}: {what}");
        }
        let form = [ok_headers[0], ok_headers[1], ("Content-Type", "application/x-www-form-urlencoded"), ok_headers[3]];
        assert_eq!(send(r.addr, m, p, &form, b).await.status, 415, "{m} {p}: form");
    }
    // Nothing happened.
    assert!(r.dir.find_domain("evil.test").await.unwrap().is_none());
    assert_eq!(r.dir.find_domain("one.test").await.unwrap().unwrap().status, rusty_directory::DomainStatus::Active);
    assert!(r.dir.find_account("evil@one.test").await.unwrap().is_none());
    assert_eq!(r.dir.find_account("alice@one.test").await.unwrap().unwrap().status, rusty_directory::AccountStatus::Active);
    assert!(r.dir.resolve("postmaster@one.test").await.is_ok());
    // Reads need a session, not the CSRF token.
    assert_eq!(send(r.addr, "GET", "/api/v1/domains", &[], "").await.status, 401);
    assert_eq!(send(r.addr, "GET", "/api/v1/domains", &[("Cookie", &c.cookie)], "").await.status, 200);
}

#[tokio::test]
async fn an_administrator_cannot_lock_itself_out_and_unknown_fields_are_refused() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(c.post_empty("/api/v1/accounts/root@home.test/disable").await.status, 400, "not the account it is signed in with");
    assert_eq!(c.post_empty("/api/v1/domains/home.test/disable").await.status, 400, "not the domain that account lives in");
    assert_eq!(c.get("/api/v1/session").await.status, 200);
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    assert_eq!(c.post_empty("/api/v1/domains/one.test/disable").await.status, 200, "another domain is fine");
    for (p, b) in [
        ("/api/v1/domains", json!({ "name": "x.test", "extra": 1 })),
        ("/api/v1/domains/one.test/accounts", json!({ "local": "x", "admin": true })),
        ("/api/v1/accounts/root@home.test/password", json!({ "password": "another-pw-12", "old": "x" })),
    ] {
        assert_eq!(c.post(p, b).await.status, 400, "{p}");
    }
    assert_eq!(c.mutate("POST", "/api/v1/domains?x=1", Some(json!({ "name": "q.test" }))).await.status, 400, "no query on a mutation");
}

#[tokio::test]
async fn changing_or_disabling_an_administrators_account_ends_its_sessions() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "boss@home.test", true).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    let boss = Client::sign_in(r.addr, "boss@home.test").await;
    assert_eq!(boss.get("/api/v1/session").await.status, 200);
    assert_eq!(root.post("/api/v1/accounts/boss@home.test/password", json!({ "password": "a-brand-new-pw-1" })).await.status, 200);
    assert_eq!(boss.get("/api/v1/session").await.status, 401, "the old session ended with the password change");
    // The person signs in again with the new password (a moment later: a session made after the revocation is honoured).
    r.now.advance(Duration::from_secs(5));
    let boss2 = {
        let body = json!({ "address": "boss@home.test", "password": "a-brand-new-pw-1" }).to_string();
        let rep = send(r.addr, "POST", "/api/v1/session", &[("Content-Type", "application/json"), ("Origin", ORIGIN)], &body).await;
        assert_eq!(rep.status, 200);
        rep.header("set-cookie").unwrap().split(';').next().unwrap().to_owned()
    };
    assert_eq!(send(r.addr, "GET", "/api/v1/session", &[("Cookie", &boss2)], "").await.status, 200, "the new session works");
    r.now.advance(Duration::from_secs(5));
    assert_eq!(root.post_empty("/api/v1/accounts/boss@home.test/disable").await.status, 200);
    assert_eq!(send(r.addr, "GET", "/api/v1/session", &[("Cookie", &boss2)], "").await.status, 401);
}
