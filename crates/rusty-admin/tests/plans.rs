//! Plans and quotas in the admin API (docs/design-plans-quota.md): a platform administrator defines a plan, an administrator puts an account of its domain on it, and the account list shows
//! the plan, the usage and the quota that applies (the plan's, unless the account or the domain has its own).

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn a_plan_is_defined_assigned_and_shown_with_the_usage_of_the_account() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(c.post("/api/v1/domains", json!({ "name": "one.test" })).await.status, 201);
    assert_eq!(c.post("/api/v1/domains/one.test/accounts", json!({ "local": "alice", "password": "alice-password-1" })).await.status, 201);

    let none = c.get("/api/v1/plans").await;
    conforms(&none.json(), "PlanList");
    assert_eq!(none.json()["items"], json!([]), "no plan until one is made (the global configuration row is not one)");
    let made = c.mutate("PUT", "/api/v1/plans", Some(json!({ "name": "small", "quota_bytes": 1_000_000, "max_message_bytes": null }))).await;
    assert_eq!((made.status, made.json()["quota_bytes"].clone()), (201, json!(1_000_000)), "{}", made.body);
    conforms(&made.json(), "Plan");
    let changed = c.mutate("PUT", "/api/v1/plans", Some(json!({ "name": "small", "quota_bytes": 2_000_000, "max_message_bytes": 50_000 }))).await;
    assert_eq!((changed.status, changed.json()["quota_bytes"].clone(), changed.json()["max_message_bytes"].clone()), (200, json!(2_000_000), json!(50_000)));

    let put = c.mutate("PUT", "/api/v1/accounts/alice@one.test/plan", Some(json!({ "plan": "small" }))).await;
    assert_eq!(put.status, 200, "{}", put.body);
    conforms(&put.json(), "AccountPlan");
    let accounts = c.get("/api/v1/domains/one.test/accounts").await;
    conforms(&accounts.json(), "AccountList");
    let alice = &accounts.json()["items"][0];
    assert_eq!((alice["plan"].clone(), alice["quota_bytes"].clone(), alice["used_bytes"].clone()), (json!("small"), json!(2_000_000), json!(0)));
    // Off the plan: no quota.
    assert_eq!(c.mutate("PUT", "/api/v1/accounts/alice@one.test/plan", Some(json!({ "plan": null }))).await.status, 200);
    let alice = c.get("/api/v1/domains/one.test/accounts").await.json()["items"][0].clone();
    assert_eq!((alice["plan"].clone(), alice["quota_bytes"].clone()), (json!(null), json!(null)));
    // Unknown plan, bad names and bad numbers are refused.
    assert_eq!(c.mutate("PUT", "/api/v1/accounts/alice@one.test/plan", Some(json!({ "plan": "nope" }))).await.status, 404);
    for bad in [json!({ "name": "Big Plan", "quota_bytes": 1 }), json!({ "name": "__global__", "quota_bytes": 1 }), json!({ "name": "p", "quota_bytes": 0 }), json!({ "name": "p", "extra": 1 })] {
        assert_eq!(c.mutate("PUT", "/api/v1/plans", Some(bad.clone())).await.status, 400, "{bad}");
    }
}

#[tokio::test]
async fn a_domain_administrator_reads_the_plans_and_assigns_its_own_accounts_but_does_not_define_plans() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "tenant@one.test", true).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(root.mutate("PUT", "/api/v1/plans", Some(json!({ "name": "small", "quota_bytes": 5_000_000 }))).await.status, 201);
    let t = Client::sign_in(r.addr, "tenant@one.test").await;
    assert_eq!(t.get("/api/v1/plans").await.json()["items"][0]["name"], "small");
    assert_eq!(t.mutate("PUT", "/api/v1/plans", Some(json!({ "name": "mine", "quota_bytes": 1 }))).await.status, 403);
    assert_eq!(t.mutate("PUT", "/api/v1/accounts/tenant@one.test/plan", Some(json!({ "plan": "small" }))).await.status, 200);
    // Another tenant's account does not exist for it.
    make_admin(&r, "x@two.test", true).await;
    assert_eq!(t.mutate("PUT", "/api/v1/accounts/x@two.test/plan", Some(json!({ "plan": "small" }))).await.status, 404);
    assert_eq!(send(r.addr, "GET", "/api/v1/plans", &[], "").await.status, 401);
}
