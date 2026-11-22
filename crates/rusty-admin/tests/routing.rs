//! Lists, the catch-all and domain aliases (docs/design-routing.md): the admin API and what the directory then does with an address.

mod common;

use common::*;
use serde_json::json;

async fn setup() -> (Rig, Client) {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(c.post("/api/v1/domains", json!({ "name": "one.test" })).await.status, 201);
    for local in ["alice", "bob"] {
        assert_eq!(c.post("/api/v1/domains/one.test/accounts", json!({ "local": local, "password": "a-long-password-1" })).await.status, 201);
    }
    (r, c)
}

#[tokio::test]
async fn a_list_takes_local_members_and_delivers_to_each() {
    let (r, c) = setup().await;
    let made = c.post("/api/v1/domains/one.test/lists", json!({ "local": "Team" })).await;
    assert_eq!(made.status, 201, "{}", made.body);
    conforms(&made.json(), "MailList");
    assert_eq!(made.json()["address"], "team@one.test");
    assert_eq!(c.post("/api/v1/domains/one.test/lists", json!({ "local": "team" })).await.status, 409);
    for who in ["alice@one.test", "bob@one.test"] {
        let added = c.post("/api/v1/domains/one.test/lists/team/members", json!({ "address": who })).await;
        assert_eq!(added.status, 201, "{}", added.body);
        conforms(&added.json(), "Members");
    }
    assert_eq!(c.post("/api/v1/domains/one.test/lists/team/members", json!({ "address": "alice@one.test" })).await.status, 409, "once only");
    assert_eq!(c.post("/api/v1/domains/one.test/lists/team/members", json!({ "address": "ghost@one.test" })).await.status, 400, "an address of a hosted domain with no such account is a typo");
    let outside = c.post("/api/v1/domains/one.test/lists/team/members", json!({ "address": "Far@Elsewhere.Test" })).await;
    assert_eq!(outside.status, 201, "{}", outside.body);
    assert_eq!(outside.json()["items"].as_array().unwrap().iter().filter(|m| m["external"] == true).count(), 1);
    assert!(outside.body.contains("far@elsewhere.test"), "stored normalised");
    let routing = c.get("/api/v1/domains/one.test/routing").await;
    conforms(&routing.json(), "Routing");
    assert_eq!(routing.json()["lists"][0]["members"], 3);
    // The directory expands the list to its two accounts.
    match r.dir.resolve("team@one.test").await.unwrap() {
        rusty_directory::Resolved::List(m) => assert_eq!(m.len(), 3, "two accounts and one outside address"),
        other => panic!("{other:?}"),
    }
    let members = c.get("/api/v1/domains/one.test/lists/team/members").await;
    let seq = members.json()["items"][0]["seq"].as_i64().unwrap();
    assert_eq!(c.mutate("DELETE", &format!("/api/v1/domains/one.test/lists/team/members/{seq}"), None).await.status, 200);
    assert_eq!(c.mutate("DELETE", "/api/v1/domains/one.test/lists/team", None).await.status, 200);
    assert_eq!(c.mutate("DELETE", "/api/v1/domains/one.test/lists/team", None).await.status, 404);
}

#[tokio::test]
async fn the_catch_all_takes_unknown_addresses_and_must_name_something_that_exists() {
    let (r, c) = setup().await;
    assert!(r.dir.resolve("nobody@one.test").await.is_err(), "no catch-all: unknown");
    assert_eq!(c.mutate("PUT", "/api/v1/domains/one.test/catch-all", Some(json!({ "local": "ghost" }))).await.status, 400, "it names nothing");
    let set = c.mutate("PUT", "/api/v1/domains/one.test/catch-all", Some(json!({ "local": "alice" }))).await;
    assert_eq!(set.status, 200, "{}", set.body);
    conforms(&set.json(), "CatchAll");
    assert!(matches!(r.dir.resolve("anything@one.test").await.unwrap(), rusty_directory::Resolved::Account(_)));
    // A real address still goes to its own owner, not to the catch-all.
    let bob = r.dir.find_account("bob@one.test").await.unwrap().unwrap().id;
    assert_eq!(r.dir.resolve("bob@one.test").await.unwrap(), rusty_directory::Resolved::Account(bob));
    assert_eq!(c.mutate("PUT", "/api/v1/domains/one.test/catch-all", Some(json!({ "local": null }))).await.status, 200);
    assert!(r.dir.resolve("anything@one.test").await.is_err());
}

#[tokio::test]
async fn a_domain_alias_delivers_as_the_domain_it_stands_for_and_only_platform_administrators_make_one() {
    let (r, c) = setup().await;
    let made = c.post("/api/v1/domains/one.test/domain-aliases", json!({ "name": "Alias.Test" })).await;
    assert_eq!(made.status, 201, "{}", made.body);
    conforms(&made.json(), "DomainAliasName");
    assert_eq!(c.post("/api/v1/domains/one.test/domain-aliases", json!({ "name": "alias.test" })).await.status, 409);
    assert_eq!(c.post("/api/v1/domains/one.test/domain-aliases", json!({ "name": "home.test" })).await.status, 409, "a hosted domain is not an alias");
    let alice = r.dir.find_account("alice@one.test").await.unwrap().unwrap().id;
    assert_eq!(r.dir.resolve("alice@alias.test").await.unwrap(), rusty_directory::Resolved::Account(alice));
    assert!(r.dir.resolve("nobody@alias.test").await.is_err());
    assert_eq!(c.get("/api/v1/domains/one.test/routing").await.json()["domain_aliases"], json!(["alias.test"]));
    // The alias gives the catch-all of its domain too.
    c.mutate("PUT", "/api/v1/domains/one.test/catch-all", Some(json!({ "local": "bob" }))).await;
    assert!(matches!(r.dir.resolve("whoever@alias.test").await.unwrap(), rusty_directory::Resolved::Account(_)));
    assert_eq!(c.mutate("DELETE", "/api/v1/domains/one.test/domain-aliases/alias.test", None).await.status, 200);
    assert!(r.dir.resolve("alice@alias.test").await.is_err());

    make_admin(&r, "tenant@one.test", true).await;
    let t = Client::sign_in(r.addr, "tenant@one.test").await;
    assert_eq!(t.post("/api/v1/domains/one.test/domain-aliases", json!({ "name": "mine.test" })).await.status, 403);
    assert_eq!(t.post("/api/v1/domains/one.test/lists", json!({ "local": "staff" })).await.status, 201, "a domain administrator manages its own lists");
    assert_eq!(t.get("/api/v1/domains/home.test/routing").await.status, 404, "and cannot see another domain");
}

#[tokio::test]
async fn accounts_are_imported_from_csv_line_by_line_with_a_dry_run_that_changes_nothing() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(c.post("/api/v1/domains", json!({ "name": "imp.test" })).await.status, 201);
    assert_eq!(c.mutate("PUT", "/api/v1/plans", Some(json!({ "name": "small", "quota_bytes": 1000000 }))).await.status, 201);
    let csv = "address,password,plan\nalice,alice-password-1,small\nbob@imp.test,\"bob,password-2\"\ncarol@elsewhere.test,carol-password-3\ndave,short\nerin,erin-password-5,nosuchplan\nalice,again-password-1\nfrank\n\n";
    let dry = c.post("/api/v1/domains/imp.test/accounts/import", json!({ "csv": csv, "dry_run": true })).await;
    assert_eq!(dry.status, 200, "{}", dry.body);
    conforms(&dry.json(), "ImportResult");
    assert_eq!((dry.json()["created"].clone(), dry.json()["invalid"].clone(), dry.json()["dry_run"].clone()), (json!(3), json!(4), json!(true)));
    assert!(r.dir.find_account("alice@imp.test").await.unwrap().is_none(), "a dry run makes nothing");
    let done = c.post("/api/v1/domains/imp.test/accounts/import", json!({ "csv": csv })).await;
    assert_eq!((done.json()["created"].clone(), done.json()["exists"].clone(), done.json()["invalid"].clone()), (json!(3), json!(0), json!(4)));
    let reasons: Vec<String> = done.json()["rows"].as_array().unwrap().iter().filter_map(|x| x["reason"].as_str().map(str::to_owned)).collect();
    assert!(reasons.contains(&"the address is not in this domain".to_owned()) && reasons.contains(&"the password is too short".to_owned()) && reasons.contains(&"no such plan".to_owned()) && reasons.contains(&"the address is listed twice".to_owned()), "{reasons:?}");
    // The accounts exist, the first has the role addresses, the plan was applied, and a second import finds them there.
    let accounts = c.get("/api/v1/domains/imp.test/accounts").await.json();
    let alice = accounts["items"].as_array().unwrap().iter().find(|a| a["address"] == "alice@imp.test").unwrap().clone();
    assert_eq!(alice["plan"], "small");
    assert!(r.dir.find_account("bob@imp.test").await.unwrap().is_some() && r.dir.find_account("frank@imp.test").await.unwrap().is_some());
    assert!(matches!(r.dir.resolve("postmaster@imp.test").await.unwrap(), rusty_directory::Resolved::Account(_)));
    let again = c.post("/api/v1/domains/imp.test/accounts/import", json!({ "csv": "alice,another-password-1\ngina,gina-password-7" })).await;
    assert_eq!((again.json()["created"].clone(), again.json()["exists"].clone()), (json!(1), json!(1)));
    // Too many lines for one request, and unknown fields, are refused.
    let big: String = (0..501).map(|i| format!("u{i}\n")).collect();
    assert_eq!(c.post("/api/v1/domains/imp.test/accounts/import", json!({ "csv": big })).await.status, 400);
    assert_eq!(c.post("/api/v1/domains/imp.test/accounts/import", json!({ "csv": "x", "extra": 1 })).await.status, 400);
}
