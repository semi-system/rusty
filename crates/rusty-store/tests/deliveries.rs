mod common;

use std::sync::Arc;

use common::*;
use rusty_core::{BlobHash, DeliveryId, Error, Flags, FolderId};
use rusty_store::{
    purge_deleted, purge_deliveries, purge_deliveries_at, BlobStore, DeliverOutcome, Mailbox,
    MailboxLimits, DELIVERY_RETENTION_SECS,
};

fn did(n: u8) -> DeliveryId {
    DeliveryId([n; 32])
}

#[tokio::test]
async fn deliver_creates_then_duplicate_returns_original() {
    let e = env().await;
    let m = raw("one", Some("one@x"));
    let a =
        e.mb.deliver(INBOX, m.clone(), Flags::empty(), did(1))
            .await
            .unwrap();
    let DeliverOutcome::Created(first) = a else {
        panic!("expected Created, got {a:?}");
    };
    // A second delivery of the same id returns the original ref, even into another folder.
    let b = e.mb.deliver(SENT, m, Flags::SEEN, did(1)).await.unwrap();
    assert_eq!(b, DeliverOutcome::Duplicate(first));
    assert_eq!(first.uid, 1);
}

#[tokio::test]
async fn duplicate_writes_no_second_message_or_change() {
    let e = env().await;
    let m = raw("one", None);
    e.mb.deliver(INBOX, m.clone(), Flags::empty(), did(1))
        .await
        .unwrap();
    let changes = e.scalar("SELECT count(*) FROM change_log").await;
    let next_msg = e.scalar("SELECT next_msg_id FROM mailboxes").await;
    let next_uid = e.scalar("SELECT next_uid FROM folders WHERE id = 1").await;
    for _ in 0..3 {
        let r =
            e.mb.deliver(INBOX, m.clone(), Flags::empty(), did(1))
                .await
                .unwrap();
        assert!(matches!(r, DeliverOutcome::Duplicate(_)));
    }
    assert_eq!(e.scalar("SELECT count(*) FROM messages").await, 1);
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 1);
    assert_eq!(e.scalar("SELECT count(*) FROM change_log").await, changes);
    assert_eq!(
        e.scalar("SELECT next_msg_id FROM mailboxes").await,
        next_msg
    );
    assert_eq!(
        e.scalar("SELECT next_uid FROM folders WHERE id = 1").await,
        next_uid
    );
}

#[tokio::test]
async fn same_id_different_content_is_conflict() {
    let e = env().await;
    e.mb.deliver(INBOX, raw("one", None), Flags::empty(), did(1))
        .await
        .unwrap();
    let err =
        e.mb.deliver(INBOX, raw("two", None), Flags::empty(), did(1))
            .await
            .unwrap_err();
    assert!(matches!(&err, Error::Conflict(m) if m == "delivery id reused with different content"));
    assert!(!format!("{err} {err:?}").contains("two"));
    assert_eq!(e.scalar("SELECT count(*) FROM messages").await, 1);
}

#[tokio::test]
async fn concurrent_duplicate_deliveries_create_one_message() {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path()).await;
    let e = env_with(store, dir).await;
    let m = raw("race", Some("race@x"));
    let mut tasks = vec![];
    for _ in 0..50 {
        let mb = e.mb.clone();
        let m = m.clone();
        tasks.push(tokio::spawn(async move {
            mb.deliver(INBOX, m, Flags::empty(), did(9)).await
        }));
    }
    let (mut created, mut dup) = (0, 0);
    for t in tasks {
        match t.await.unwrap().unwrap() {
            DeliverOutcome::Created(_) => created += 1,
            DeliverOutcome::Duplicate(_) => dup += 1,
        }
    }
    assert_eq!((created, dup), (1, 49));
    assert_eq!(e.scalar("SELECT count(*) FROM messages").await, 1);
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 1);
    assert_eq!(
        e.scalar("SELECT count(*) FROM change_log WHERE kind = 'message' AND op = 'create'")
            .await,
        1
    );
}

#[tokio::test]
async fn different_mailboxes_same_id_are_independent() {
    let e = env().await;
    let other = e.other("bob").await;
    let m = raw("one", None);
    let a =
        e.mb.deliver(INBOX, m.clone(), Flags::empty(), did(1))
            .await
            .unwrap();
    let b = other
        .deliver(INBOX, m.clone(), Flags::empty(), did(1))
        .await
        .unwrap();
    assert!(matches!(a, DeliverOutcome::Created(_)));
    assert!(matches!(b, DeliverOutcome::Created(_)));
    // Same id with DIFFERENT content in a fresh third mailbox is not a conflict there either.
    let third = e.other("carol").await;
    let c = third
        .deliver(INBOX, raw("different", None), Flags::empty(), did(1))
        .await
        .unwrap();
    assert!(matches!(c, DeliverOutcome::Created(_)));
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 3);
}

#[tokio::test]
async fn deliver_to_foreign_folder_is_not_found() {
    let e = env().await;
    let other = e.other("bob").await;
    // Folder ids 1..=5 exist in both mailboxes; a folder id absent here is NotFound.
    let r =
        e.mb.deliver(FolderId(99), raw("x", None), Flags::empty(), did(1))
            .await;
    assert!(matches!(r, Err(Error::NotFound(_))));
    let r = other
        .deliver(FolderId(99), raw("x", None), Flags::empty(), did(1))
        .await;
    assert!(matches!(r, Err(Error::NotFound(_))));
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 0);
    assert_eq!(e.scalar("SELECT count(*) FROM messages").await, 0);

    // A folder created only in the other mailbox is unreachable from this one.
    let f = other.create_folder(None, "only-bob").await.unwrap();
    let r =
        e.mb.deliver(f, raw("x", None), Flags::empty(), did(1))
            .await;
    assert!(matches!(r, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn failed_deliver_leaves_no_delivery_row_so_retry_creates() {
    let e = env().await;
    let r =
        e.mb.deliver(FolderId(99), raw("x", None), Flags::empty(), did(1))
            .await;
    assert!(r.is_err());
    let r =
        e.mb.deliver(INBOX, raw("x", None), Flags::empty(), did(1))
            .await
            .unwrap();
    assert!(matches!(r, DeliverOutcome::Created(_)));
}

#[tokio::test]
async fn delivery_row_cascades_with_message_purge() {
    let e = env().await;
    let m =
        e.mb.deliver(INBOX, raw("gone", None), Flags::empty(), did(1))
            .await
            .unwrap();
    let DeliverOutcome::Created(r) = m else {
        panic!()
    };
    e.mb.expunge(r.id).await.unwrap();
    // Soft-deleted: the row is still there, so a retry is still a duplicate.
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 1);
    assert_eq!(purge_deleted(&e.store, i64::MAX).await.unwrap(), 1);
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 0);
}

#[tokio::test]
async fn purge_deliveries_removes_only_old_rows() {
    let e = env().await;
    for n in 1..=3u8 {
        e.mb.deliver(INBOX, raw(&format!("m{n}"), None), Flags::empty(), did(n))
            .await
            .unwrap();
    }
    sqlx::query("UPDATE deliveries SET created_at = 100 WHERE delivery_id = ?")
        .bind(did(1).0.to_vec())
        .execute(e.store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE deliveries SET created_at = 1000 WHERE delivery_id = ?")
        .bind(did(2).0.to_vec())
        .execute(e.store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE deliveries SET created_at = 5000 WHERE delivery_id = ?")
        .bind(did(3).0.to_vec())
        .execute(e.store.pool())
        .await
        .unwrap();
    // Cutoff is strict: created_at < now - retention.
    assert_eq!(
        purge_deliveries_at(&e.store, 600, 100)
            .await
            .unwrap(),
        1
    );
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 2);
    assert_eq!(
        purge_deliveries_at(&e.store, 600, 100)
            .await
            .unwrap(),
        0
    );
    assert_eq!(purge_deliveries(&e.store, 5000).await.unwrap(), 1);
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 1);
    // Messages are untouched.
    assert_eq!(e.scalar("SELECT count(*) FROM messages").await, 3);
    // Default retention with the DB clock: rows created just now survive.
    assert_eq!(DELIVERY_RETENTION_SECS, 7 * 86400);
    sqlx::query("UPDATE deliveries SET created_at = strftime('%s','now')")
        .execute(e.store.pool())
        .await
        .unwrap();
    assert_eq!(
        rusty_store::purge_deliveries_default(&e.store)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn purge_deliveries_batches_beyond_1000() {
    let e = env().await;
    let r =
        e.mb.deliver(INBOX, raw("x", None), Flags::empty(), did(1))
            .await
            .unwrap();
    let DeliverOutcome::Created(r) = r else {
        panic!()
    };
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2500) \
         INSERT INTO deliveries (mailbox_id, delivery_id, message_id, blob_hash, created_at) \
         SELECT ?, randomblob(32), ?, hex(randomblob(32)), 1 FROM n",
    )
    .bind(e.mb.id().0)
    .bind(r.id.0)
    .execute(e.store.pool())
    .await
    .unwrap();
    // The real delivery row (created now) survives; the 2500 synthetic ones go.
    let total = e.scalar("SELECT count(*) FROM deliveries").await;
    // 2500 synthetic rows + the one real delivery row.
    assert_eq!(total, 2501);
    assert_eq!(
        i64::try_from(purge_deliveries(&e.store, 100).await.unwrap()).unwrap(),
        total - 1
    );
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 1);
}

#[tokio::test]
async fn quota_and_size_limits_apply_to_deliver() {
    let e = env().await;
    let small = Mailbox::open(
        e.store.clone(),
        e.blobs.clone() as Arc<dyn BlobStore>,
        e.account,
    )
    .await
    .unwrap()
    .with_limits(MailboxLimits {
        max_message_bytes: 64,
    });
    let r = small
        .deliver(INBOX, sized(65), Flags::empty(), did(1))
        .await;
    assert!(matches!(r, Err(Error::Quota(_))));
    let r = small
        .deliver(INBOX, bytes::Bytes::new(), Flags::empty(), did(1))
        .await;
    assert!(matches!(r, Err(Error::Invalid(_))));
    // An oversize retry of an existing delivery is still Quota, not a silent duplicate.
    small
        .deliver(INBOX, sized(64), Flags::empty(), did(2))
        .await
        .unwrap();
    let r = small
        .deliver(INBOX, sized(65), Flags::empty(), did(2))
        .await;
    assert!(matches!(r, Err(Error::Quota(_))));
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 1);
    assert!(
        e.blob_files().len() <= 1,
        "oversize input must not be written"
    );
}

#[tokio::test]
async fn append_stays_non_idempotent() {
    let e = env().await;
    let m = raw("same", None);
    let a = e.mb.append(INBOX, m.clone(), Flags::empty()).await.unwrap();
    let b = e.mb.append(INBOX, m, Flags::empty()).await.unwrap();
    assert_ne!(a.id, b.id);
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 0);
}

#[test]
fn delivery_id_derive_is_stable_and_field_separated() {
    let h = BlobHash::of(b"content");
    let a = DeliveryId::derive(&h, "alice@example.com", "s1");
    assert_eq!(a, DeliveryId::derive(&h, "alice@example.com", "s1"));
    assert_ne!(a, DeliveryId::derive(&h, "alice@example.com", "s2"));
    assert_ne!(a, DeliveryId::derive(&h, "bob@example.com", "s1"));
    assert_ne!(
        a,
        DeliveryId::derive(&BlobHash::of(b"other"), "alice@example.com", "s1")
    );
    assert_ne!(
        DeliveryId::derive(&h, "ab", "c"),
        DeliveryId::derive(&h, "a", "bc")
    );
    assert_ne!(
        DeliveryId::derive(&h, "", "x"),
        DeliveryId::derive(&h, "x", "")
    );
    // Hex round trip and strict parsing.
    let hex = a.to_hex();
    assert_eq!(hex.len(), 64);
    assert_eq!(DeliveryId::from_hex(&hex).unwrap(), a);
    assert!(DeliveryId::from_hex(&hex.to_uppercase()).is_err());
    assert!(DeliveryId::from_hex(&hex[..62]).is_err());
    assert!(DeliveryId::from_hex(&format!("{hex}00")).is_err());
    assert!(DeliveryId::from_hex(&"g".repeat(64)).is_err());
    let err = DeliveryId::from_hex("zz-secret").unwrap_err();
    assert!(!format!("{err} {err:?}").contains("secret"));
}

#[tokio::test]
async fn deliver_at_stamps_created_at_and_deliver_uses_db_clock() {
    let e = env().await;
    e.mb.deliver_at(INBOX, raw("a", None), Flags::empty(), did(1), 4242).await.unwrap();
    assert_eq!(e.scalar("SELECT created_at FROM deliveries").await, 4242);
    e.mb.deliver(INBOX, raw("b", None), Flags::empty(), did(2)).await.unwrap();
    let db_now = e.scalar("SELECT CAST(strftime('%s','now') AS INTEGER)").await;
    let at = e
        .scalar("SELECT created_at FROM deliveries WHERE delivery_id = x'0202020202020202020202020202020202020202020202020202020202020202'")
        .await;
    assert!((db_now - at).abs() <= 5);
}
