//! `sealed_secrets` (admin web design, section 14.1): the storage of secrets that were sealed to the outbound service's
//! public key. The store never sees a plaintext and never opens anything; what it guarantees is the shape: one row per
//! (owner kind, owner id, purpose), a write replaces the whole row, a reader and a writer are different facades (the admin
//! writes and can list metadata, it has no call that returns a ciphertext), and nothing outlives its owner domain.
mod common;

use common::backends::*;
use rusty_core::{DomainId, Error};
use rusty_store::{SealedReader, SealedWriter};

const KEY: &str = "0123456789abcdef";
const KEY2: &str = "fedcba9876543210";

fn blob(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn writer(b: &Backend) -> SealedWriter {
    b.sealed_writer()
}

fn reader(b: &Backend) -> SealedReader {
    b.sealed_reader()
}

async fn domain(b: &Backend, name: &str) -> DomainId {
    b.dir.create_domain(name).await.unwrap()
}

#[tokio::test]
async fn a_sealed_secret_is_stored_and_read_back_byte_for_byte() {
    for_each_backend(|b| async move {
        let n = b.name;
        let d = domain(&b, "sealed.test").await;
        // Every byte value, including zeros: it is a binary blob, not text.
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let meta = writer(&b).put_at("domain", d.0, "dkim-private-key", KEY, &data, 100).await.unwrap();
        assert_eq!((meta.owner_kind.as_str(), meta.owner_id, meta.purpose.as_str(), meta.key_id.as_str(), meta.size, meta.created_at, meta.updated_at), ("domain", d.0, "dkim-private-key", KEY, 1000, 100, 100), "{n}");
        let row = reader(&b).get("domain", d.0, "dkim-private-key").await.unwrap().unwrap();
        assert_eq!(row.sealed, data, "{n}");
        assert_eq!(row.key_id, KEY, "{n}");
        assert!(reader(&b).get("domain", d.0, "relay-password").await.unwrap().is_none(), "{n}");
        assert!(reader(&b).get("domain", d.0 + 1, "dkim-private-key").await.unwrap().is_none(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn a_write_replaces_the_whole_row_and_keeps_when_it_was_first_made() {
    for_each_backend(|b| async move {
        let n = b.name;
        let d = domain(&b, "replace.test").await;
        let w = writer(&b);
        w.put_at("domain", d.0, "relay-password", KEY, &blob(80, 1), 100).await.unwrap();
        let meta = w.put_at("domain", d.0, "relay-password", KEY2, &blob(120, 2), 250).await.unwrap();
        assert_eq!((meta.key_id.as_str(), meta.size, meta.created_at, meta.updated_at), (KEY2, 120, 100, 250), "{n}");
        let row = reader(&b).get("domain", d.0, "relay-password").await.unwrap().unwrap();
        assert_eq!((row.sealed, row.key_id.as_str()), (blob(120, 2), KEY2), "{n}: the old blob is gone, nothing is merged");
        assert_eq!(w.meta("domain", d.0).await.unwrap().len(), 1, "{n}");
    })
    .await;
}

#[tokio::test]
async fn owners_and_purposes_are_isolated_and_listed_without_ciphertext() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (d1, d2) = (domain(&b, "one.test").await, domain(&b, "two.test").await);
        let w = writer(&b);
        w.put_at("domain", d1.0, "relay-password", KEY, &blob(70, 1), 10).await.unwrap();
        w.put_at("domain", d1.0, "dkim-private-key", KEY, &blob(90, 2), 20).await.unwrap();
        w.put_at("domain", d2.0, "relay-password", KEY2, &blob(70, 3), 30).await.unwrap();
        w.put_at("tenant", d1.0, "relay-password", KEY, &blob(70, 4), 40).await.unwrap();
        let m = w.meta("domain", d1.0).await.unwrap();
        assert_eq!(m.iter().map(|x| (x.purpose.as_str(), x.size)).collect::<Vec<_>>(), [("dkim-private-key", 90), ("relay-password", 70)], "{n}: ordered by purpose");
        assert_eq!(w.meta("domain", d2.0).await.unwrap().len(), 1, "{n}");
        assert_eq!(w.meta("tenant", d1.0).await.unwrap().len(), 1, "{n}: another kind with the same id is another owner");
        assert_eq!(reader(&b).get("domain", d2.0, "relay-password").await.unwrap().unwrap().sealed, blob(70, 3), "{n}");
        assert!(w.meta("domain", 987_654).await.unwrap().is_empty(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn delete_removes_one_secret_and_says_whether_there_was_one() {
    for_each_backend(|b| async move {
        let n = b.name;
        let d = domain(&b, "delete.test").await;
        let w = writer(&b);
        w.put_at("domain", d.0, "relay-password", KEY, &blob(70, 1), 10).await.unwrap();
        w.put_at("domain", d.0, "dkim-private-key", KEY, &blob(70, 2), 10).await.unwrap();
        assert!(w.delete("domain", d.0, "relay-password").await.unwrap(), "{n}");
        assert!(!w.delete("domain", d.0, "relay-password").await.unwrap(), "{n}");
        assert!(reader(&b).get("domain", d.0, "relay-password").await.unwrap().is_none(), "{n}");
        assert!(reader(&b).get("domain", d.0, "dkim-private-key").await.unwrap().is_some(), "{n}: the other purpose stays");
    })
    .await;
}

#[tokio::test]
async fn input_is_validated_before_the_database() {
    for_each_backend(|b| async move {
        let n = b.name;
        let w = writer(&b);
        let ok = blob(70, 1);
        let long = "a".repeat(33);
        for (kind, purpose, key, data) in [
            ("", "p", KEY, &ok[..]),
            ("Domain", "p", KEY, &ok[..]),
            ("do main", "p", KEY, &ok[..]),
            (long.as_str(), "p", KEY, &ok[..]),
            ("domain", "", KEY, &ok[..]),
            ("domain", "p/q", KEY, &ok[..]),
            ("domain", "p", "short", &ok[..]),
            ("domain", "p", "0123456789ABCDEF", &ok[..]),
            ("domain", "p", "0123456789abcdeg", &ok[..]),
            ("domain", "p", KEY, &[][..]),
            ("domain", "p", KEY, &ok[..10]),
        ] {
            let r = w.put_at(kind, 1, purpose, key, data, 10).await;
            assert!(matches!(r, Err(Error::Invalid(_))), "{n}: {kind:?} {purpose:?} {key:?} {} bytes -> {r:?}", data.len());
        }
        let too_big = blob(rusty_store::MAX_SEALED_BYTES + 1, 0);
        assert!(matches!(w.put_at("domain", 1, "p", KEY, &too_big, 10).await, Err(Error::Invalid(_))), "{n}");
        assert!(w.put_at("domain", 1, "p", KEY, &blob(rusty_store::MAX_SEALED_BYTES, 0), 10).await.is_ok(), "{n}: exactly the limit");
        assert!(matches!(reader(&b).get("Bad Kind", 1, "p").await, Err(Error::Invalid(_))), "{n}");
        assert!(matches!(w.meta("", 1).await, Err(Error::Invalid(_))), "{n}");
        assert!(matches!(w.delete("domain", 1, "").await, Err(Error::Invalid(_))), "{n}");
    })
    .await;
}

#[tokio::test]
async fn the_database_refuses_what_the_store_would_have_refused() {
    for_each_backend(|b| async move {
        let n = b.name;
        // Straight SQL, as a bug or a careless operator would write it.
        for (kind, key, size) in [("BAD", KEY, 70), ("domain", "zz", 70), ("domain", KEY, 3)] {
            let sql = format!("INSERT INTO sealed_secrets (owner_kind, owner_id, purpose, key_id, sealed, created_at, updated_at) VALUES ($1, 1, 'p', $2, decode(repeat('ab', {size}), 'hex'), 1, 1)");
            let r = try_execute(&b, &sql, &[Bind::S(kind), Bind::S(key)]).await;
            assert!(r.is_err(), "{n}: {kind} {key} {size}");
        }
    })
    .await;
}

#[tokio::test]
async fn secrets_of_a_domain_go_with_the_domain_and_nothing_else_does() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (gone, kept) = (domain(&b, "gone.test").await, domain(&b, "kept.test").await);
        let w = writer(&b);
        w.put_at("domain", gone.0, "relay-password", KEY, &blob(70, 1), 10).await.unwrap();
        w.put_at("domain", kept.0, "relay-password", KEY, &blob(70, 2), 10).await.unwrap();
        // A secret of another kind that happens to have the same id belongs to something else.
        w.put_at("tenant", gone.0, "relay-password", KEY, &blob(70, 3), 10).await.unwrap();
        run(&b, &format!("DELETE FROM domains WHERE id = {}", gone.0)).await;
        assert!(w.meta("domain", gone.0).await.unwrap().is_empty(), "{n}: nothing outlives its domain");
        assert_eq!(w.meta("domain", kept.0).await.unwrap().len(), 1, "{n}");
        assert_eq!(w.meta("tenant", gone.0).await.unwrap().len(), 1, "{n}");
    })
    .await;
}

#[tokio::test]
async fn concurrent_writes_to_one_secret_leave_exactly_one_whole_blob() {
    for_each_backend(|b| async move {
        let n = b.name;
        let d = domain(&b, "race.test").await;
        let w = writer(&b);
        let mut tasks = Vec::new();
        for i in 0..16u8 {
            let w = w.clone();
            tasks.push(tokio::spawn(async move { w.put_at("domain", d.0, "relay-password", KEY, &blob(70 + i as usize, i), 10 + i64::from(i)).await }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        let row = reader(&b).get("domain", d.0, "relay-password").await.unwrap().unwrap();
        let i = row.sealed.len() - 70;
        assert_eq!(row.sealed, blob(70 + i, i as u8), "{n}: one writer's blob, not a mixture");
        assert_eq!(w.meta("domain", d.0).await.unwrap().len(), 1, "{n}");
    })
    .await;
}
