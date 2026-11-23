//! The platform administrator's search of the mail (docs/design-admin-search.md): off until the platform setting is on, platform administrators only, validated, audited, and read back.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn the_search_is_off_by_default_for_platform_administrators_only_validated_audited_and_read_back() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "tenant@one.test", true).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    let body = json!({ "subject": "invoice" });

    // Off until the platform setting is on.
    assert_eq!(root.mutate("POST", "/api/v1/search", Some(body.clone())).await.status, 409, "off by default");
    let settings = root.get("/api/v1/settings").await.json();
    assert_eq!(settings["admin_search"], json!(false));
    let mut on = settings.clone();
    on["admin_search"] = json!(true);
    assert_eq!(root.mutate("PUT", "/api/v1/settings", Some(on)).await.status, 200);
    assert_eq!(root.get("/api/v1/settings").await.json()["admin_search"], json!(true));

    // A domain administrator may not, whatever the setting.
    let tenant = Client::sign_in(r.addr, "tenant@one.test").await;
    assert_eq!(tenant.mutate("POST", "/api/v1/search", Some(body.clone())).await.status, 403);
    assert_eq!(tenant.get("/api/v1/search/1").await.status, 403);

    // What is asked has to be something: a criterion, sensible texts and dates.
    for bad in [json!({}), json!({ "domain": "one.test" }), json!({ "subject": "  " }), json!({ "from": "a".repeat(201) }), json!({ "since": 10, "until": 5 }), json!({ "account": "no-at-sign", "subject": "x" }), json!({ "subject": "x", "extra": 1 }), json!({ "subject": "line\nbreak" })] {
        assert_eq!(root.mutate("POST", "/api/v1/search", Some(bad.clone())).await.status, 400, "{bad}");
    }

    let made = root.mutate("POST", "/api/v1/search", Some(body)).await;
    assert_eq!(made.status, 201, "{}", made.body);
    conforms(&made.json(), "SearchQueued");
    let id = made.json()["id"].as_i64().unwrap();
    let got = root.get(&format!("/api/v1/search/{id}")).await;
    assert_eq!(got.status, 200, "{}", got.body);
    conforms(&got.json(), "Search");
    assert_eq!((got.json()["state"].clone(), got.json()["results"].clone()), (json!("queued"), json!([])));
    assert_eq!(root.get("/api/v1/search/999999").await.status, 404);
    assert_eq!(root.get("/api/v1/search/abc").await.status, 404);

    // The request is in the audit log with what was asked for.
    let audit = root.get("/api/v1/audit").await.json();
    let entry = audit["items"].as_array().unwrap().iter().find(|i| i["path"] == json!("/api/v1/search")).expect("the search was audited").clone();
    assert!(entry["detail"].as_str().unwrap().contains("invoice"), "{entry}");
}
