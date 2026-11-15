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

// ------------------------------------------------------------------------------------------------ pools of backends

fn backend(name: &str, bucket: &str) -> Value {
    json!({ "name": name, "endpoint": "https://s3.example-cloud.com", "region": "eu-west-3", "bucket": bucket, "path_style": true, "access_key": "AKIAEXAMPLEKEY", "secret_key": "super-secret-key-material" })
}

#[tokio::test]
async fn a_pool_is_built_by_a_platform_administrator_with_sealed_keys_and_a_domain_administrator_sees_its_state_only() {
    use rusty_core::storage_policy::BACKEND_SECRET_OWNER;
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "boss@one.test", true).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    let path = "/api/v1/domains/one.test/storage";

    // No pool yet: no backend can be added.
    assert_eq!(c.post(&format!("{path}/backends"), backend("a", "bucket-a")).await.status, 404);
    let put = c.mutate("PUT", &format!("{path}/pool"), Some(json!({ "copies": 2, "sync_copies": 1, "placement": "ordered", "read": "ordered" }))).await;
    assert_eq!(put.status, 200, "{}", put.body);
    conforms(&put.json(), "StorageState");
    let st = &put.json()["storage"];
    assert_eq!((st["kind"].as_str(), st["ready"].as_bool()), (Some("pool"), Some(false)));
    assert_eq!((st["policy"]["copies"].as_i64(), st["policy"]["sync_copies"].as_i64(), st["policy"]["placement"].as_str()), (Some(2), Some(1), Some("ordered")));
    assert_eq!(st["backends"].as_array().map(Vec::len), Some(0));

    for (n, bucket) in [("a", "bucket-a"), ("b", "bucket-b")] {
        let add = c.post(&format!("{path}/backends"), backend(n, bucket)).await;
        assert_eq!(add.status, 200, "{}", add.body);
        conforms(&add.json(), "StorageState");
        assert!(!add.body.contains("AKIAEXAMPLEKEY") && !add.body.contains("super-secret"), "the keys are never returned");
    }
    let got = c.get(path).await.json();
    conforms(&got, "StorageState");
    for b in got["storage"]["backends"].as_array().unwrap() {
        conforms(b, "StorageBackend");
        assert_eq!((b["access_key_set"].as_bool(), b["secret_key_set"].as_bool(), b["state"].as_str(), b["weight"].as_i64(), b["objects"].as_i64()), (Some(true), Some(true), Some("active"), Some(100), Some(0)));
    }
    // The keys are sealed to each backend, bound to it.
    let pools = rusty_store::BlobPoolAdmin::with_repos(r.store.repos().blob_stores.clone(), r.store.repos().blob_pool.clone());
    let store = r.dir.find_domain("one.test").await.unwrap().unwrap();
    let pool = pools.pool_of(store.id).await.unwrap().unwrap();
    let backends = pools.backends(pool.id).await.unwrap();
    for be in &backends {
        for (purpose, plain) in [(ACCESS_KEY_PURPOSE, "AKIAEXAMPLEKEY"), (SECRET_KEY_PURPOSE, "super-secret-key-material")] {
            let row = r.outbound.repos().sealed.get(BACKEND_SECRET_OWNER, be.id.0, purpose).await.unwrap().expect("sealed");
            let ctx = Context::new(BACKEND_SECRET_OWNER, be.id.0, purpose).unwrap();
            assert_eq!(&r.unsealer.unseal(&ctx, &row.sealed).unwrap()[..], plain.as_bytes());
            let other = Context::new(BACKEND_SECRET_OWNER, be.id.0 + 1, purpose).unwrap();
            assert!(r.unsealer.unseal(&other, &row.sealed).is_err(), "bound to its backend");
        }
    }

    // What a tenant may not choose is refused, and nothing is added.
    for bad in [
        json!({ "name": "c", "endpoint": "https://127.0.0.1", "region": "r", "bucket": "bbb", "access_key": "a", "secret_key": "b" }),
        json!({ "name": "C", "endpoint": "https://s3.example-cloud.com", "region": "r", "bucket": "bbb", "access_key": "a", "secret_key": "b" }),
        json!({ "name": "c", "endpoint": "https://s3.example-cloud.com", "region": "r", "bucket": "bbb", "weight": 0, "access_key": "a", "secret_key": "b" }),
        json!({ "name": "c", "endpoint": "https://s3.example-cloud.com", "region": "r", "bucket": "bbb", "access_key": "", "secret_key": "b" }),
        json!({ "name": "c", "endpoint": "https://s3.example-cloud.com", "region": "r", "bucket": "bbb", "access_key": "a", "secret_key": "b", "unknown": 1 }),
    ] {
        assert_eq!(c.post(&format!("{path}/backends"), bad).await.status, 400);
    }
    assert_eq!(c.post(&format!("{path}/backends"), backend("a", "bucket-x")).await.status, 409, "a name is used once");
    assert_eq!(pools.backends(pool.id).await.unwrap().len(), 2);
    for i in 0..6 {
        assert_eq!(c.post(&format!("{path}/backends"), backend(&format!("x{i}"), &format!("bucket-x{i}"))).await.status, 200);
    }
    assert_eq!(c.post(&format!("{path}/backends"), backend("nine", "bucket-nine")).await.status, 409, "at most eight");
    for i in 0..6 {
        assert_eq!(c.mutate("DELETE", &format!("{path}/backends/x{i}"), None).await.status, 200);
    }

    // A domain administrator sees the state and nothing of the settings.
    let boss = Client::sign_in(r.addr, "boss@one.test").await;
    let seen = boss.get(path).await;
    assert_eq!(seen.status, 200, "{}", seen.body);
    conforms(&seen.json(), "StorageState");
    for b in seen.json()["storage"]["backends"].as_array().unwrap() {
        assert!(b["settings"].is_null() && b["access_key_set"].is_null() && b["objects"].is_null(), "{b}");
    }
    assert!(!seen.body.contains("s3.example-cloud.com") && !seen.body.contains("bucket-a"), "{}", seen.body);
    assert_eq!(boss.post(&format!("{path}/backends"), backend("z", "bucket-z")).await.status, 403);
    assert_eq!(boss.mutate("PUT", &format!("{path}/pool"), Some(json!({ "copies": 1 }))).await.status, 403);
    assert_eq!(boss.mutate("PUT", &format!("{path}/backends/a"), Some(json!({ "weight": 5 }))).await.status, 403);
    assert_eq!(boss.mutate("DELETE", &format!("{path}/backends/a"), None).await.status, 403);
    assert_eq!(boss.post(&format!("{path}/backends/a/keys"), json!({ "access_key": "a", "secret_key": "b" })).await.status, 403);

    // State and weight; the check is queued for the pool; new keys make the backend unchecked.
    let set = c.mutate("PUT", &format!("{path}/backends/b"), Some(json!({ "state": "draining", "weight": 300 }))).await;
    assert_eq!(set.status, 200, "{}", set.body);
    let b = set.json()["storage"]["backends"].as_array().unwrap().iter().find(|b| b["name"] == "b").cloned().unwrap();
    assert_eq!((b["state"].as_str(), b["weight"].as_i64()), (Some("draining"), Some(300)));
    assert_eq!(c.mutate("PUT", &format!("{path}/backends/b"), Some(json!({ "state": "gone" }))).await.status, 400);
    assert_eq!(c.mutate("PUT", &format!("{path}/backends/b"), Some(json!({}))).await.status, 400);
    assert_eq!(c.mutate("PUT", &format!("{path}/backends/nosuch"), Some(json!({ "weight": 5 }))).await.status, 404);
    assert_eq!(c.post_empty(&format!("{path}/check")).await.status, 200);
    assert_eq!(r.count_jobs("storage.check").await, 1);
    let be = backends.iter().find(|x| x.name == "b").unwrap();
    pools.record_check(be.id, "ok").await.unwrap();
    let keys = c.post(&format!("{path}/backends/b/keys"), json!({ "access_key": "AKIANEW", "secret_key": "new-secret" })).await;
    assert_eq!(keys.status, 200, "{}", keys.body);
    assert_eq!(pools.backend(be.id).await.unwrap().unwrap().check_status, "unchecked");
    let row = r.outbound.repos().sealed.get(BACKEND_SECRET_OWNER, be.id.0, ACCESS_KEY_PURPOSE).await.unwrap().unwrap();
    let ctx = Context::new(BACKEND_SECRET_OWNER, be.id.0, ACCESS_KEY_PURPOSE).unwrap();
    assert_eq!(&r.unsealer.unseal(&ctx, &row.sealed).unwrap()[..], b"AKIANEW");

    // A backend that holds an object cannot be removed; after it was emptied it can.
    let hash = rusty_core::BlobHash::of(b"x");
    r.outbound.repos().blob_pool.add_locations(pool.id, &hash, &[be.id]).await.unwrap();
    assert_eq!(c.mutate("DELETE", &format!("{path}/backends/b"), None).await.status, 409);
    assert_eq!(c.get(path).await.json()["storage"]["backends"][1]["objects"].as_i64(), Some(1));
    r.outbound.repos().blob_pool.remove_location(pool.id, &hash, be.id).await.unwrap();
    assert_eq!(c.mutate("DELETE", &format!("{path}/backends/b"), None).await.status, 200);
    assert_eq!(pools.backends(pool.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_domain_with_a_single_store_is_not_made_a_pool_and_a_pool_needs_the_sealing_key_for_its_backends() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "single.test" })).await;
    assert_eq!(c.mutate("PUT", "/api/v1/domains/single.test/storage", Some(body("https://s3.example-cloud.com", "tenant-mail"))).await.status, 200);
    assert_eq!(c.mutate("PUT", "/api/v1/domains/single.test/storage/pool", Some(json!({ "copies": 2 }))).await.status, 409);
    // Wrong settings: copies 9, sync_copies above copies, an unknown setting.
    c.post("/api/v1/domains", json!({ "name": "p.test" })).await;
    for bad in [json!({ "copies": 9 }), json!({ "copies": 1, "sync_copies": 2 }), json!({ "placement": "random" }), json!({ "colour": "red" })] {
        assert_eq!(c.mutate("PUT", "/api/v1/domains/p.test/storage/pool", Some(bad)).await.status, 400);
    }
    let none = rig_without_sealing().await;
    make_admin(&none, "root@home.test", false).await;
    let n = Client::sign_in(none.addr, "root@home.test").await;
    n.post("/api/v1/domains", json!({ "name": "p.test" })).await;
    assert_eq!(n.mutate("PUT", "/api/v1/domains/p.test/storage/pool", Some(json!({}))).await.status, 200);
    assert_eq!(n.post("/api/v1/domains/p.test/storage/backends", backend("a", "bucket-a")).await.status, 503);
    assert_eq!(n.get("/api/v1/domains/p.test/storage").await.json()["storage"]["backends"].as_array().map(Vec::len), Some(0), "no half-made backend");
}
