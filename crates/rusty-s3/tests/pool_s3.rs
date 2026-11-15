//! A pool of real S3 buckets (RustFS): the pool code of `rusty-store` over `S3BlobStore` backends. A backend whose bucket does not exist stands for a backend that is down; copies are
//! made on the others, reads and deletions reach every real copy.
//!
//! Needs the S3 server of `scripts/s3-up.sh` (`RUSTY_TEST_S3_*`) and Postgres (`RUSTY_TEST_PG_URL`); missing: skipped, unless `RUSTY_REQUIRE_S3=1`.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use rusty_core::{BlobHash, Result};
use rusty_s3::{Credentials, DirectConnector, S3BlobStore, S3Client, S3Config};
use rusty_store::fixtures::pg_db::{pg_admin_url, PgTestDb};
use rusty_store::repo::BackendRow;
use rusty_store::{BackendOpener, BlobStore, PoolPolicy, PoolStore, Store, StoreConfig};

struct Server {
    endpoint: String,
    creds: Credentials,
}

fn server() -> Option<Server> {
    let get = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
    match (get("RUSTY_TEST_S3_ENDPOINT"), get("RUSTY_TEST_S3_KEY"), get("RUSTY_TEST_S3_SECRET")) {
        (Some(endpoint), Some(access_key), Some(secret_key)) => Some(Server { endpoint, creds: Credentials { access_key, secret_key } }),
        _ => {
            assert!(std::env::var("RUSTY_REQUIRE_S3").as_deref() != Ok("1"), "RUSTY_REQUIRE_S3=1 but RUSTY_TEST_S3_* are not set");
            None
        }
    }
}

fn unique(tag: &str) -> String {
    let n = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
    format!("rt-{tag}-{n:x}").to_lowercase().chars().take(60).collect()
}

fn client(s: &Server, bucket: &str) -> S3Client {
    let cfg = S3Config { endpoint: s.endpoint.clone(), region: "us-east-1".into(), bucket: bucket.into(), prefix: String::new(), path_style: true, deadline: Duration::from_secs(20) };
    S3Client::new(cfg, s.creds.clone(), Arc::new(DirectConnector { timeout: Duration::from_secs(3) }), None).unwrap()
}

/// Opens a backend by the bucket named in its settings.
struct Buckets(Server);

#[async_trait]
impl BackendOpener for Buckets {
    async fn open(&self, row: &BackendRow) -> Result<Arc<dyn BlobStore>> {
        let v: serde_json::Value = serde_json::from_str(&row.config).unwrap();
        Ok(Arc::new(S3BlobStore::new(client(&self.0, v["bucket"].as_str().unwrap()))))
    }
}

#[tokio::test]
async fn a_mirror_over_real_buckets_skips_a_missing_one_reads_from_any_copy_and_deletes_every_copy() {
    let Some(s) = server() else { return };
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    let repos = store.repos();
    let dom = rusty_directory::Directory::with_repo(repos.directory.clone()).create_domain("pool.test").await.unwrap();
    let pool = repos.blob_stores.create_for_domain(dom, "pool", r#"{"copies":2,"placement":"ordered","read":"ordered"}"#).await.unwrap();

    // Two real buckets and one that does not exist (a backend that is down), the missing one first.
    let (b1, b2, missing) = (unique("a"), unique("b"), unique("none"));
    for b in [&b1, &b2] {
        client(&s, b).create_bucket().await.expect("make the bucket");
    }
    let mut rows = Vec::new();
    for (name, bucket) in [("down", &missing), ("one", &b1), ("two", &b2)] {
        let id = repos.blob_pool.add_backend(pool, name, "s3", &format!(r#"{{"bucket":"{bucket}"}}"#), 100).await.unwrap();
        repos.blob_pool.record_backend_check(id, "ok", None).await.unwrap();
        rows.push(repos.blob_pool.backend(id).await.unwrap().unwrap());
    }
    let opener = Buckets(Server { endpoint: s.endpoint.clone(), creds: s.creds.clone() });
    let mut opened = Vec::new();
    for r in rows {
        let st = opener.open(&r).await.unwrap();
        opened.push((r, st));
    }
    let p = PoolStore::new(pool, PoolPolicy::parse(r#"{"copies":2,"placement":"ordered","read":"ordered"}"#).unwrap(), repos.blob_pool.clone(), opened).unwrap();

    // Both copies land on the two buckets that exist, whatever the missing one does.
    let body = Bytes::from(vec![9u8; 3 * 1024 * 1024 + 5]);
    let h = p.put(body.clone()).await.unwrap();
    let key = |h: &BlobHash| format!("{}/{}/{}", &h.as_str()[0..2], &h.as_str()[2..4], h.as_str());
    for b in [&b1, &b2] {
        assert!(client(&s, b).head(&key(&h)).await.unwrap().is_some(), "a copy in {b}");
    }
    assert_eq!(repos.blob_pool.locations(pool, &h).await.unwrap().len(), 2);
    assert_eq!(p.get(&h).await.unwrap(), body);

    // Lose the copy of the first bucket: the read comes from the second, and repair puts the first back.
    client(&s, &b1).delete(&key(&h)).await.unwrap();
    assert_eq!(p.get(&h).await.unwrap(), body);
    assert_eq!(repos.blob_pool.locations(pool, &h).await.unwrap().len(), 1, "the missing copy is forgotten");

    // Garbage collection reaches every copy that exists; the missing backend holds nothing.
    assert!(p.delete_if_older(&h, SystemTime::now() + Duration::from_secs(3600)).await.unwrap());
    for b in [&b1, &b2] {
        assert!(client(&s, b).head(&key(&h)).await.unwrap().is_none(), "no copy left in {b}");
    }
    assert!(repos.blob_pool.locations(pool, &h).await.unwrap().is_empty());
}
