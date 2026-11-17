//! Search, status filter and page size of the lists (domains, accounts, aliases, administrators): the filter narrows the list and its `total`, a page ends with a cursor that continues it, a
//! character that is a wildcard elsewhere is only a character here, a malformed query is refused whole, and a filter never reaches past what the administrator may see.

mod common;

use common::*;
use serde_json::{json, Value};

fn addresses(list: &Value) -> Vec<String> {
    list["items"].as_array().unwrap().iter().map(|i| i["address"].as_str().unwrap().to_owned()).collect()
}

async fn rig_with_accounts() -> (Rig, Client) {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    for local in ["alice", "albert", "bob", "carl", "dana", "a_b", "axb"] {
        let made = c.post("/api/v1/domains/one.test/accounts", json!({ "local": local, "password": "a-long-password-1" })).await;
        assert_eq!(made.status, 201, "{local}: {}", made.body);
    }
    (r, c)
}

#[tokio::test]
async fn accounts_are_searched_by_a_substring_and_by_status_and_the_total_follows_the_filter() {
    let (_r, c) = rig_with_accounts().await;
    let all = c.get("/api/v1/domains/one.test/accounts").await;
    assert_eq!(all.status, 200);
    conforms(&all.json(), "AccountList");
    assert_eq!(all.json()["total"], 7);
    let al = c.get("/api/v1/domains/one.test/accounts?q=AL").await.json();
    assert_eq!(addresses(&al), ["alice@one.test", "albert@one.test"], "lower-cased, a substring of the name");
    assert_eq!(al["total"], 2);
    assert_eq!(c.post_empty("/api/v1/accounts/bob@one.test/lock").await.status, 200);
    assert_eq!(c.post_empty("/api/v1/accounts/carl@one.test/disable").await.status, 200);
    let locked = c.get("/api/v1/domains/one.test/accounts?status=locked").await.json();
    assert_eq!((addresses(&locked), locked["total"].clone()), (vec!["bob@one.test".to_owned()], json!(1)));
    assert_eq!(c.get("/api/v1/domains/one.test/accounts?status=disabled").await.json()["total"], 1);
    assert_eq!(c.get("/api/v1/domains/one.test/accounts?status=active").await.json()["total"], 5);
    // Both together, and a search with no answer is an empty list, not an error.
    assert_eq!(c.get("/api/v1/domains/one.test/accounts?q=a&status=active").await.json()["total"], 5);
    assert_eq!(c.get("/api/v1/domains/one.test/accounts?q=bo&status=active").await.json()["total"], 0);
    let none = c.get("/api/v1/domains/one.test/accounts?q=nobody-has-this").await;
    assert_eq!((none.status, none.json()["items"].as_array().unwrap().len(), none.json()["next"].clone()), (200, 0, Value::Null));
}

#[tokio::test]
async fn a_character_that_is_a_wildcard_elsewhere_is_only_a_character_here() {
    let (_r, c) = rig_with_accounts().await;
    let under = c.get("/api/v1/domains/one.test/accounts?q=_").await.json();
    assert_eq!(addresses(&under), ["a_b@one.test"], "`_` is not `any one character`");
    // `%` is the escape character of the query, so a literal one is refused rather than searched.
    assert_eq!(c.get("/api/v1/domains/one.test/accounts?q=%25").await.status, 400);
    assert_eq!(c.get("/api/v1/domains/one.test/accounts?q=a%2Bb").await.json()["total"], 0, "`+` is an ordinary character of an address");
    let plus = c.get("/api/v1/domains/one.test/accounts?q=a%40one").await;
    assert_eq!(plus.status, 200, "an address may contain the @ (escaped as the browser sends it)");
}

#[tokio::test]
async fn a_page_has_the_size_asked_for_and_a_cursor_that_continues_it_with_the_same_total() {
    let (_r, c) = rig_with_accounts().await;
    let mut seen = Vec::new();
    let mut url = "/api/v1/domains/one.test/accounts?limit=3".to_owned();
    let mut pages = 0;
    loop {
        let page = c.get(&url).await.json();
        assert_eq!(page["total"], 7, "the total is of the whole list on every page");
        assert!(page["items"].as_array().unwrap().len() <= 3);
        seen.extend(addresses(&page));
        pages += 1;
        match page["next"].as_i64() {
            Some(n) => url = format!("/api/v1/domains/one.test/accounts?limit=3&after={n}"),
            None => break,
        }
    }
    assert_eq!((pages, seen.len()), (3, 7));
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 7, "no account twice");
    // A filter and a cursor together.
    let first = c.get("/api/v1/domains/one.test/accounts?q=a&limit=2").await.json();
    assert_eq!((first["items"].as_array().unwrap().len(), first["total"].clone()), (2, json!(6)));
    let next = first["next"].as_i64().unwrap();
    let rest = c.get(&format!("/api/v1/domains/one.test/accounts?q=a&limit=2&after={next}")).await.json();
    assert!(addresses(&rest).iter().all(|a| !addresses(&first).contains(a)));
}

#[tokio::test]
async fn a_malformed_query_is_refused_whole_and_nothing_is_searched() {
    let (_r, c) = rig_with_accounts().await;
    for q in [
        "limit=0", "limit=101", "limit=x", "limit=%2B5", "after=-1", "status=bogus", "status=locked&status=active", "q=a&q=b", "q=a b", "q=a%00b", "q=%ZZ", "q=%C3%28", "q=%C3%A9", "x=1", "q", "&", "limit=5&&q=a",
        // A name too long to be an address.
        &format!("q={}", "a".repeat(65)),
    ] {
        let r = c.get(&format!("/api/v1/domains/one.test/accounts?{q}")).await;
        assert_eq!(r.status, 400, "{q}: {}", r.body);
    }
    // The aliases take no status.
    assert_eq!(c.get("/api/v1/domains/one.test/aliases?status=active").await.status, 400);
}

#[tokio::test]
async fn aliases_are_searched_by_their_own_name_or_that_of_their_account() {
    let (_r, c) = rig_with_accounts().await;
    assert_eq!(c.post("/api/v1/domains/one.test/aliases", json!({ "local": "sales", "target": "alice@one.test" })).await.status, 201);
    assert_eq!(c.post("/api/v1/domains/one.test/aliases", json!({ "local": "help", "target": "bob@one.test" })).await.status, 201);
    let all = c.get("/api/v1/domains/one.test/aliases").await.json();
    conforms(&all, "AliasList");
    assert_eq!(all["total"], 4, "postmaster and abuse come with the first account");
    let by_name = c.get("/api/v1/domains/one.test/aliases?q=sal").await.json();
    assert_eq!(by_name["total"], 1);
    let by_target = c.get("/api/v1/domains/one.test/aliases?q=bob").await.json();
    assert_eq!(by_target["items"][0]["alias"], "help@one.test");
}

#[tokio::test]
async fn domains_and_administrators_are_searched_and_filtered_too() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    for d in ["alpha.test", "beta.test", "alphabet.test"] {
        c.post("/api/v1/domains", json!({ "name": d })).await;
    }
    c.post_empty("/api/v1/domains/beta.test/disable").await;
    let all = c.get("/api/v1/domains").await.json();
    conforms(&all, "DomainList");
    assert_eq!(all["total"], 4, "with home.test");
    assert_eq!(c.get("/api/v1/domains?q=alpha").await.json()["total"], 2);
    assert_eq!(c.get("/api/v1/domains?status=disabled").await.json()["total"], 1);
    assert_eq!(c.get("/api/v1/domains?q=alpha&status=disabled").await.json()["total"], 0);
    assert_eq!(c.get("/api/v1/domains?limit=2").await.json()["items"].as_array().unwrap().len(), 2);

    make_admin(&r, "boss@alpha.test", true).await;
    let admins = c.get("/api/v1/admins").await.json();
    conforms(&admins, "AdminList");
    assert_eq!(admins["total"], 2);
    assert_eq!(c.get("/api/v1/admins?q=boss").await.json()["total"], 1);
    assert_eq!(c.get("/api/v1/admins?q=alpha.test").await.json()["items"][0]["address"], "boss@alpha.test");
    assert_eq!(c.get("/api/v1/admins?status=active").await.status, 400, "administrators have no status filter");
}

#[tokio::test]
async fn a_filter_never_reaches_past_the_administrators_own_domain() {
    let (r, root) = rig_with_accounts().await;
    root.post("/api/v1/domains", json!({ "name": "two.test" })).await;
    root.post("/api/v1/domains/two.test/accounts", json!({ "local": "zed", "password": "zed-password-12" })).await;
    make_admin(&r, "boss@one.test", true).await;
    let boss = Client::sign_in(r.addr, "boss@one.test").await;
    // Another tenant's lists are not found, whatever the filter.
    for q in ["", "?q=zed", "?status=active"] {
        assert_eq!(boss.get(&format!("/api/v1/domains/two.test/accounts{q}")).await.status, 404, "{q}");
    }
    // Its own domain list holds its own domain, and a filter that does not match it is an empty list; a search for the other domain finds nothing.
    let own = boss.get("/api/v1/domains").await.json();
    assert_eq!((own["items"].as_array().unwrap().len(), own["total"].clone()), (1, json!(1)));
    assert_eq!(boss.get("/api/v1/domains?q=two").await.json()["total"], 0);
    assert_eq!(boss.get("/api/v1/domains?q=one").await.json()["total"], 1);
    assert_eq!(boss.get("/api/v1/domains?status=disabled").await.json()["total"], 0);
    // The administrators it lists are its own domain's.
    let admins = boss.get("/api/v1/admins?q=root").await.json();
    assert_eq!(admins["total"], 0, "the platform administrator is not in its list");
}

#[tokio::test]
async fn the_list_of_accounts_says_when_each_last_signed_in_and_never_for_one_that_did_not() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains/home.test/accounts", json!({ "local": "quiet", "password": "a-long-password-1" })).await;
    // The sign-in is recorded by a task of its own (it never delays the sign-in): look until it has been.
    let mut root_time = Value::Null;
    for _ in 0..50 {
        let list = c.get("/api/v1/domains/home.test/accounts").await.json();
        conforms(&list, "AccountList");
        let root = list["items"].as_array().unwrap().iter().find(|a| a["address"] == "root@home.test").unwrap().clone();
        conforms(&root, "Account");
        root_time = root["last_login_at"].clone();
        if !root_time.is_null() {
            let quiet = list["items"].as_array().unwrap().iter().find(|a| a["address"] == "quiet@home.test").unwrap();
            assert!(quiet["last_login_at"].is_null(), "an account that never signed in has no time: {quiet}");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(root_time.as_i64().is_some_and(|t| t > 0), "the administrator signed in and the list says when: {root_time}");
}
