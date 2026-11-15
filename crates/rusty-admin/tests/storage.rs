//! The storage endpoints: only a platform administrator sets a domain's S3 store, the keys are sealed and never returned, a domain administrator sees status only, the policy of
//! `rustyctl storage` applies, and the responses have their contract shapes.
mod common;

use common::*;
use rusty_core::storage_policy::{ACCESS_KEY_PURPOSE, SECRET_KEY_PURPOSE, STORE_SECRET_OWNER};
use rusty_seal::Context;
use serde_json::{json, Value};

fn body(endpoint: &str, bucket: &str) -> Value {
    json!({ "endpoint": endpoint, "region": "eu-west-3", "bucket": bucket, "prefix": "mail/a", "path_style": true, "access_key": "AKIAEXAMPLEKEY", "secret_key": "super-secret-key-material" })
}

#[tokio::test]
async fn a_platform_administrator_sets_the_store_with_sealed_keys_and_it_is_not_used_before_a_check() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    let none = c.get("/api/v1/domains/one.test/storage").await;
    assert_eq!(none.status, 200, "{}", none.body);
    conforms(&none.json(), "StorageState");
    assert!(none.json()["storage"].is_null(), "no store of its own: the platform store");

    let put = c.mutate("PUT", "/api/v1/domains/one.test/storage", Some(body("https://S3.Example-Cloud.com", "tenant-mail"))).await;
    assert_eq!(put.status, 200, "{}", put.body);
    conforms(&put.json(), "StorageState");
    let st = &put.json()["storage"];
    assert_eq!((st["kind"].as_str(), st["ready"].as_bool(), st["check_status"].as_str()), (Some("s3"), Some(false), Some("unchecked")), "not ready until a check passed");
    assert_eq!((st["settings"]["endpoint"].as_str(), st["settings"]["bucket"].as_str(), st["settings"]["path_style"].as_bool()), (Some("https://s3.example-cloud.com"), Some("tenant-mail"), Some(true)));
    assert_eq!((st["access_key_set"].as_bool(), st["secret_key_set"].as_bool()), (Some(true), Some(true)));
    assert!(!put.body.contains("AKIAEXAMPLEKEY") && !put.body.contains("super-secret"), "the keys are never returned");
    let got = c.get("/api/v1/domains/one.test/storage").await;
    assert!(!got.body.contains("AKIA") && !got.body.contains("super-secret"));

    // Both keys are sealed to this store: only the mail service's unsealing key opens them, and only for this store and purpose.
    let admin_store = rusty_store::BlobStoreAdmin::with_repo(r.store.repos().blob_stores.clone());
    let store = admin_store.for_domain(r.dir.find_domain("one.test").await.unwrap().unwrap().id).await.unwrap().unwrap();
    for (purpose, plain) in [(ACCESS_KEY_PURPOSE, "AKIAEXAMPLEKEY"), (SECRET_KEY_PURPOSE, "super-secret-key-material")] {
        let row = r.outbound.repos().sealed.get(STORE_SECRET_OWNER, store.id.0, purpose).await.unwrap().expect("sealed");
        let ctx = Context::new(STORE_SECRET_OWNER, store.id.0, purpose).unwrap();
        let opened = r.unsealer.unseal(&ctx, &row.sealed).unwrap();
        assert_eq!(&opened[..], plain.as_bytes());
        let other = Context::new(STORE_SECRET_OWNER, store.id.0 + 1, purpose).unwrap();
        assert!(r.unsealer.unseal(&other, &row.sealed).is_err(), "bound to its store");
    }

    // Check: queued for the mail service; a store that is not an S3 store of the domain has none.
    let q = c.post_empty("/api/v1/domains/one.test/storage/check").await;
    assert_eq!(q.status, 200, "{}", q.body);
    conforms(&q.json(), "Queued");
    assert_eq!(r.count_jobs("storage.check").await, 1);
    c.post("/api/v1/domains", json!({ "name": "two.test" })).await;
    assert_eq!(c.post_empty("/api/v1/domains/two.test/storage/check").await.status, 404, "nothing to check");

    // The same settings again replace the keys and make the store unchecked once more.
    admin_store.record_check(store.id, "ok").await.unwrap();
    assert_eq!(c.get("/api/v1/domains/one.test/storage").await.json()["storage"]["ready"], true, "after a passed check");
    let again = c.mutate("PUT", "/api/v1/domains/one.test/storage", Some(body("https://s3.example-cloud.com", "tenant-mail"))).await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.json()["storage"]["ready"], false, "new keys: unchecked again");
}

#[tokio::test]
async fn what_a_tenant_may_not_choose_is_refused_and_nothing_is_stored() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    for endpoint in ["http://s3.example-cloud.com", "https://127.0.0.1", "https://169.254.169.254", "https://s3.internal", "https://s3.example-cloud.com:22", "https://s3.example-cloud.com/x", "https://u:p@s3.example-cloud.com"] {
        let put = c.mutate("PUT", "/api/v1/domains/one.test/storage", Some(body(endpoint, "tenant-mail"))).await;
        assert_eq!(put.status, 400, "{endpoint}: {}", put.body);
    }
    assert_eq!(c.mutate("PUT", "/api/v1/domains/one.test/storage", Some(body("https://s3.example-cloud.com", "UPPER"))).await.status, 400);
    for (k, v) in [("access_key", ""), ("secret_key", ""), ("access_key", "a\nb")] {
        let mut b = body("https://s3.example-cloud.com", "tenant-mail");
        b[k] = json!(v);
        assert_eq!(c.mutate("PUT", "/api/v1/domains/one.test/storage", Some(b)).await.status, 400, "{k} {v:?}");
    }
    let mut extra = body("https://s3.example-cloud.com", "tenant-mail");
    extra["unknown"] = json!(1);
    assert_eq!(c.mutate("PUT", "/api/v1/domains/one.test/storage", Some(extra)).await.status, 400, "unknown fields are refused");
    assert!(c.get("/api/v1/domains/one.test/storage").await.json()["storage"].is_null(), "nothing was stored");
}

#[tokio::test]
async fn a_domain_administrator_sees_the_status_of_its_store_and_can_change_nothing() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "boss@one.test", true).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    root.post("/api/v1/domains", json!({ "name": "two.test" })).await;
    root.mutate("PUT", "/api/v1/domains/one.test/storage", Some(body("https://s3.example-cloud.com", "tenant-mail"))).await;
    let boss = Client::sign_in(r.addr, "boss@one.test").await;
    let got = boss.get("/api/v1/domains/one.test/storage").await;
    assert_eq!(got.status, 200, "{}", got.body);
    conforms(&got.json(), "StorageState");
    let st = &got.json()["storage"];
    assert_eq!((st["kind"].as_str(), st["ready"].as_bool()), (Some("s3"), Some(false)));
    assert!(st["settings"].is_null() && st["access_key_set"].is_null() && st["secret_key_set"].is_null(), "no settings for a domain administrator");
    assert!(!got.body.contains("s3.example-cloud.com") && !got.body.contains("tenant-mail"), "{}", got.body);
    // Changing, checking: platform administrators only. Another tenant's domain is the same 404 as a missing one.
    assert_eq!(boss.mutate("PUT", "/api/v1/domains/one.test/storage", Some(body("https://s3.example-cloud.com", "other-bucket"))).await.status, 403);
    assert_eq!(boss.post_empty("/api/v1/domains/one.test/storage/check").await.status, 403);
    assert_eq!(boss.get("/api/v1/domains/two.test/storage").await.status, 404);
    assert_eq!(boss.mutate("PUT", "/api/v1/domains/two.test/storage", Some(body("https://s3.example-cloud.com", "other-bucket"))).await.status, 403);
}

#[tokio::test]
async fn without_a_sealing_key_nothing_is_stored_and_the_answer_is_unavailable() {
    let r = rig_without_sealing().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    assert_eq!(c.mutate("PUT", "/api/v1/domains/one.test/storage", Some(body("https://s3.example-cloud.com", "tenant-mail"))).await.status, 503);
    assert!(c.get("/api/v1/domains/one.test/storage").await.json()["storage"].is_null(), "no store row without keys to seal");
}
