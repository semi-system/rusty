//! `S3BlobStore` against a real S3 server (RustFS in CI and on the developer's machine): the whole `BlobStore` contract, the error categories, the limits.
//!
//! The server is named in the environment: `RUSTY_TEST_S3_ENDPOINT` (`http://127.0.0.1:19000`), `RUSTY_TEST_S3_KEY`, `RUSTY_TEST_S3_SECRET`. Missing: the tests are skipped,
//! unless `RUSTY_REQUIRE_S3=1` (CI), where it is a failure.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use rusty_core::{BlobHash, Error};
use rusty_s3::{Credentials, DirectConnector, S3BlobStore, S3Client, S3Config, S3Fail};
use rusty_store::BlobStore;

struct Server {
    endpoint: String,
    creds: Credentials,
}

fn server() -> Option<Server> {
    let get = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
    match (get("RUSTY_TEST_S3_ENDPOINT"), get("RUSTY_TEST_S3_KEY"), get("RUSTY_TEST_S3_SECRET")) {
        (Some(endpoint), Some(access_key), Some(secret_key)) => Some(Server { endpoint, creds: Credentials { access_key, secret_key } }),
        _ => {
            assert!(std::env::var("RUSTY_REQUIRE_S3").as_deref() != Ok("1"), "RUSTY_REQUIRE_S3=1 but RUSTY_TEST_S3_ENDPOINT/KEY/SECRET are not set");
            None
        }
    }
}

fn unique(tag: &str) -> String {
    let n = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
    format!("rt-{tag}-{n:x}").to_lowercase().chars().take(60).collect()
}

fn config(s: &Server, bucket: &str, prefix: &str) -> S3Config {
    S3Config { endpoint: s.endpoint.clone(), region: "us-east-1".into(), bucket: bucket.into(), prefix: prefix.into(), path_style: true, deadline: Duration::from_secs(20) }
}

fn client(s: &Server, bucket: &str, prefix: &str) -> S3Client {
    S3Client::new(config(s, bucket, prefix), s.creds.clone(), Arc::new(DirectConnector { timeout: Duration::from_secs(3) }), None).unwrap()
}

async fn store(s: &Server, tag: &str, prefix: &str) -> S3BlobStore {
    let bucket = unique(tag);
    let c = client(s, &bucket, prefix);
    c.create_bucket().await.expect("make the test bucket");
    S3BlobStore::new(c)
}

#[tokio::test]
async fn put_get_range_head_and_delete_round_trip_with_the_content_as_the_name() {
    let Some(s) = server() else { return };
    let st = store(&s, "rt", "").await;
    let body = Bytes::from(vec![7u8; 5 * 1024 * 1024 + 13]);
    let h = st.put(body.clone()).await.unwrap();
    assert_eq!(h, BlobHash::of(&body));
    assert_eq!(st.put(body.clone()).await.unwrap(), h, "a put is idempotent");
    assert_eq!(st.get(&h).await.unwrap(), body, "a body of several MiB comes back whole");
    // A range of the object, through the client.
    let key = format!("{}/{}/{}", &h.as_str()[0..2], &h.as_str()[2..4], h.as_str());
    let small = st.put(Bytes::from((0..=255u8).collect::<Vec<_>>())).await.unwrap();
    let small_key = format!("{}/{}/{}", &small.as_str()[0..2], &small.as_str()[2..4], small.as_str());
    assert_eq!(&st.client().get(&small_key, Some((10, 19)), 100).await.unwrap()[..], &(10..=19u8).collect::<Vec<_>>()[..]);
    let meta = st.client().head(&key).await.unwrap().unwrap();
    assert_eq!(meta.size, body.len() as u64);
    assert!(st.client().head("no/such/object").await.unwrap().is_none());
    // Too recent to delete, then old enough.
    assert!(!st.delete_if_older(&h, SystemTime::now() - Duration::from_secs(3600)).await.unwrap(), "younger than the cutoff stays");
    assert!(st.get(&h).await.is_ok());
    assert!(st.delete_if_older(&h, SystemTime::now() + Duration::from_secs(3600)).await.unwrap());
    assert!(!st.delete_if_older(&h, SystemTime::now() + Duration::from_secs(3600)).await.unwrap(), "already gone");
    assert!(matches!(st.get(&h).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn an_object_that_does_not_match_its_name_is_never_returned() {
    let Some(s) = server() else { return };
    let st = store(&s, "tamper", "").await;
    let real = st.put(Bytes::from_static(b"the real body")).await.unwrap();
    // Swap the object for other bytes under the same name, as a tenant with write access to the bucket could.
    let key = format!("{}/{}/{}", &real.as_str()[0..2], &real.as_str()[2..4], real.as_str());
    st.client().put(&key, Bytes::from_static(b"somebody else's mail")).await.unwrap();
    let err = st.get(&real).await.unwrap_err();
    assert!(matches!(err, Error::Storage { .. }), "{err:?}");
}

#[tokio::test]
async fn listing_walks_shards_pages_and_ignores_what_is_not_ours() {
    let Some(s) = server() else { return };
    let st = store(&s, "list", "tenant/a").await;
    let mut hashes = Vec::new();
    for i in 0..40 {
        hashes.push(st.put(Bytes::from(format!("body number {i}"))).await.unwrap());
    }
    // Things that are not blobs of this store: odd names under the prefix are skipped, and objects outside the prefix are never listed.
    st.client().put("junk.txt", Bytes::from_static(b"x")).await.unwrap();
    st.client().put("aa/bb/not-a-hash", Bytes::from_static(b"x")).await.unwrap();
    let other = S3BlobStore::new(client(&s, st.client().config().bucket.as_str(), "tenant/b"));
    other.put(Bytes::from_static(b"the other tenant")).await.unwrap();

    let mut seen = Vec::new();
    for shard in 0..=255u8 {
        for e in st.list_shard(shard).await.unwrap() {
            assert_eq!(&e.hash.as_str()[..2], format!("{shard:02x}"));
            assert!(e.modified_unix > 1_700_000_000);
            seen.push(e.hash);
        }
    }
    seen.sort();
    hashes.sort();
    assert_eq!(seen, hashes, "exactly the forty blobs of this prefix");
    assert_eq!(st.list().await.unwrap().len(), 40);
    assert_eq!(other.list().await.unwrap().len(), 1, "the other tenant's prefix is its own");

    // Paging: more objects than one page holds.
    let c = st.client();
    for i in 0..1005 {
        c.put(&format!("paging/{i:04}"), Bytes::from_static(b"p")).await.unwrap();
    }
    let first = c.list("paging/", None).await.unwrap();
    assert_eq!(first.entries.len(), 1000);
    let second = c.list("paging/", Some(first.next.as_deref().expect("a next page"))).await.unwrap();
    assert_eq!(second.entries.len(), 5);
    assert!(second.next.is_none());
    assert_eq!(first.entries[0].key, "paging/0000");
}

#[tokio::test]
async fn failures_are_sorted_into_their_categories() {
    let Some(s) = server() else { return };
    let st = store(&s, "err", "").await;
    st.put(Bytes::from_static(b"x")).await.unwrap();
    let bucket = st.client().config().bucket.clone();

    // A wrong secret: refused as an authentication failure, and the store maps it to a permanent fault.
    let bad = S3BlobStore::new(S3Client::new(config(&s, &bucket, ""), Credentials { access_key: s.creds.access_key.clone(), secret_key: "not-the-secret".into() }, Arc::new(DirectConnector { timeout: Duration::from_secs(3) }), None).unwrap());
    // (A HEAD reply has no body to name the reason, so it can only say 403: Denied. A GET names it.)
    assert_eq!(bad.client().get("a/b/c", None, 1000).await.unwrap_err(), S3Fail::Auth);
    assert_eq!(bad.client().head("a/b/c").await.unwrap_err(), S3Fail::Denied);
    assert!(matches!(bad.put(Bytes::from_static(b"y")).await, Err(Error::Storage { .. })));
    // A bucket that does not exist.
    let nobucket = client(&s, &unique("missing"), "");
    assert_eq!(nobucket.put("k", Bytes::from_static(b"y")).await.unwrap_err(), S3Fail::NoBucket);
    // A port nobody listens on: temporary, so the store says Busy (SMTP 451, IMAP [UNAVAILABLE]).
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let mut cfg = config(&s, &bucket, "");
    cfg.endpoint = format!("http://127.0.0.1:{port}");
    let down = S3BlobStore::new(S3Client::new(cfg, s.creds.clone(), Arc::new(DirectConnector { timeout: Duration::from_secs(1) }), None).unwrap());
    assert_eq!(down.client().head("a/b/c").await.unwrap_err(), S3Fail::Connect);
    assert!(matches!(down.get(&BlobHash::of(b"x")).await, Err(Error::Busy(_))));
    // A server that accepts and never answers: the deadline ends the call.
    let hang = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hang_port = hang.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = hang.accept().await {
            held.push(sock);
        }
    });
    let mut cfg = config(&s, &bucket, "");
    cfg.endpoint = format!("http://127.0.0.1:{hang_port}");
    cfg.deadline = Duration::from_millis(400);
    let slow = S3Client::new(cfg, s.creds.clone(), Arc::new(DirectConnector { timeout: Duration::from_secs(1) }), None).unwrap();
    assert_eq!(slow.head("a/b/c").await.unwrap_err(), S3Fail::Timeout);
    // A reply larger than the limit of the call.
    let big = st.put(Bytes::from(vec![1u8; 100_000])).await.unwrap();
    let key = format!("{}/{}/{}", &big.as_str()[0..2], &big.as_str()[2..4], big.as_str());
    assert_eq!(st.client().get(&key, None, 1000).await.unwrap_err(), S3Fail::TooLarge);
}

#[tokio::test]
async fn the_gc_lock_is_exclusive_and_the_store_clock_is_the_servers() {
    let Some(s) = server() else { return };
    let st = store(&s, "lock", "").await;
    let lock = st.try_lock_gc().await.unwrap();
    assert!(matches!(st.try_lock_gc().await, Err(Error::Conflict(_))), "a second collector is refused");
    drop(lock);
    assert!(st.try_lock_gc().await.is_ok(), "released on drop");
    st.put(Bytes::from_static(b"x")).await.unwrap();
    let skew = st.fs_now().duration_since(SystemTime::now()).unwrap_or_else(|e| e.duration());
    assert!(skew < Duration::from_secs(120), "the store clock is within two minutes of ours: {skew:?}");
    assert_eq!(st.sweep_temp_before(SystemTime::now()).await.unwrap(), 0);
}

#[tokio::test]
async fn the_store_check_passes_on_a_working_bucket_leaves_nothing_behind_and_names_what_is_wrong() {
    use rusty_s3::probe::probe;
    let Some(s) = server() else { return };
    let st = store(&s, "probe", "tenant/a").await;
    let bucket = st.client().config().bucket.clone();
    // A working bucket: the check passes twice (a second run does not trip over the first) and removes its probe object.
    probe(st.client(), 1).await.unwrap();
    probe(st.client(), 2).await.unwrap();
    assert!(st.client().list(".rusty-probe/", None).await.unwrap().entries.is_empty(), "no probe object is left");
    // What is wrong, as a category: the secret key, a bucket that does not exist.
    let wrong = S3Client::new(config(&s, &bucket, "tenant/a"), Credentials { access_key: s.creds.access_key.clone(), secret_key: "not-the-secret".into() }, Arc::new(DirectConnector { timeout: Duration::from_secs(3) }), None).unwrap();
    assert_eq!(probe(&wrong, 3).await.unwrap_err(), S3Fail::Auth);
    let missing = client(&s, &unique("nobucket"), "tenant/a");
    assert_eq!(probe(&missing, 4).await.unwrap_err(), S3Fail::NoBucket);
}
