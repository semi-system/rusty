//! GcRepo conformance on both backends: message purge, delivery/capability sweeps, blob row
//! collection with the database-clock grace, the zero_since invariant.
mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use std::sync::Arc;

use bytes::Bytes;
use common::backends::*;
use rusty_core::{BlobHash, DeliveryId, Flags};
use rusty_store::Gc;

const HOUR: Duration = Duration::from_secs(3600);

fn hex_of(h: &BlobHash) -> String {
    h.as_str().to_owned()
}

/// An unreferenced blob: the file exists (fresh mtime) and the row has the given age.
async fn blob(b: &Backend, name: &str, zero_since: Option<i64>) -> BlobHash {
    let h = b.blobs.put(Bytes::from(format!("blob {name}"))).await.unwrap();
    insert_blob_row(b, h.as_str(), 10).await;
    if let Some(t) = zero_since {
        set_zero_since(b, h.as_str(), t).await;
    }
    h
}

#[tokio::test]
async fn grace_boundary_is_exact_at_the_cutoff() {
    for_each_backend(|b| async move {
        let n = b.name;
        let gc = b.gc();
        let now_secs = 1_000_000i64;
        let cutoff = now_secs - 3600;
        let at = blob(&b, "at", Some(cutoff)).await;
        let before = blob(&b, "before", Some(cutoff - 1)).await;
        let after = blob(&b, "after", Some(cutoff + 1)).await;
        let now = UNIX_EPOCH + Duration::from_secs(now_secs as u64);
        let rep = gc.collect_blobs_at(b.blobs.as_ref(), HOUR, now).await.unwrap();
        assert_eq!(rep.rows_deleted, 1, "{n}: {rep:?}");
        assert!(!blob_row_exists(&b, &hex_of(&before)).await, "{n} cutoff-1 goes");
        assert!(blob_row_exists(&b, &hex_of(&at)).await, "{n} cutoff stays");
        assert!(blob_row_exists(&b, &hex_of(&after)).await, "{n} cutoff+1 stays");
    })
    .await;
}

#[tokio::test]
async fn database_clock_path_uses_the_db_now() {
    for_each_backend(|b| async move {
        let n = b.name;
        let now = db_clock(&b).await;
        let old = blob(&b, "old", Some(now - 3601 - 5)).await;
        let young = blob(&b, "young", Some(now - 3500)).await;
        let rep = b.gc().collect_blobs(b.blobs.as_ref(), HOUR).await.unwrap();
        assert_eq!(rep.rows_deleted, 1, "{n}: {rep:?}");
        assert!(!blob_row_exists(&b, old.as_str()).await, "{n}");
        assert!(blob_row_exists(&b, young.as_str()).await, "{n}");
        // The minimum grace is enforced.
        let r = b.gc().collect_blobs(b.blobs.as_ref(), Duration::from_secs(3599)).await;
        assert!(matches!(r, Err(rusty_core::Error::Invalid(_))), "{n}");
    })
    .await;
}

/// The cutoff follows the DATABASE clock: skewed by a year each way while rows are aged
/// relative to the real time. A cutoff from the process clock gives the opposite answers.
#[tokio::test]
async fn cutoff_follows_a_skewed_database_clock() {
    for (skew, expect, label) in [(31_536_000i64, 2, "ahead"), (-31_536_000, 0, "behind")] {
        for_each_backend(|b| async move {
            let n = b.name;
            let now = db_clock(&b).await;
            blob(&b, "old", Some(now - 3 * 3600)).await;
            blob(&b, "young", Some(now - 1800)).await;
            let gc = Gc::new(Arc::new(SkewedGc { inner: b.grepo.clone(), skew }));
            let rep = gc.collect_blobs(b.blobs.as_ref(), HOUR).await.unwrap();
            assert_eq!(rep.rows_deleted, expect, "{n} {label}: {rep:?}");
        })
        .await;
    }
}

#[tokio::test]
async fn row_deleted_file_removed_after_commit_and_orphans_handled() {
    for_each_backend(|b| async move {
        let n = b.name;
        let now = db_clock(&b).await;
        let gone = blob(&b, "gone", Some(now - 7200)).await;
        let kept = blob(&b, "kept", Some(now)).await;
        // A file with no row: removed only once older than the cutoff.
        let orphan = b.blobs.put(Bytes::from_static(b"orphan")).await.unwrap();
        let future = SystemTime::now() + Duration::from_secs(10 * 86400);
        let rep = b.gc().collect_blobs_at(b.blobs.as_ref(), HOUR, future).await.unwrap();
        assert_eq!(rep.rows_deleted, 2, "{n}: {rep:?}");
        // Everything is far older than the cutoff at `future`: rows and orphan files go.
        for h in [&gone, &kept, &orphan] {
            assert!(b.blobs.get(h).await.is_err(), "{n} file {h:?} removed");
        }
        // A fresh orphan within grace stays.
        let fresh = b.blobs.put(Bytes::from_static(b"fresh orphan")).await.unwrap();
        let rep = b.gc().collect_blobs(b.blobs.as_ref(), HOUR).await.unwrap();
        assert_eq!(rep.files_deleted, 0, "{n}: {rep:?}");
        assert!(b.blobs.get(&fresh).await.is_ok(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn referenced_blob_is_never_deleted_even_with_a_corrupt_refcount() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "gcref").await;
        mb.append(INBOX, common::raw("held", None), Flags::default()).await.unwrap();
        // Content addressing: putting the same bytes again names the referenced blob.
        let h = b.blobs.put(common::raw("held", None)).await.unwrap();
        // Corrupt: the refcount says 0 and the grace is long over, but a message references it.
        run(&b, &format!("UPDATE blobs SET refcount = 0 WHERE hash = {}", hash_lit(&b, h.as_str()))).await;
        set_zero_since(&b, h.as_str(), 0).await;
        let future = SystemTime::now() + Duration::from_secs(10 * 86400);
        let rep = b.gc().collect_blobs_at(b.blobs.as_ref(), HOUR, future).await.unwrap();
        assert_eq!(rep.rows_deleted, 0, "{n}: {rep:?}");
        assert!(!rep.errors.is_empty(), "{n}: the failed delete is reported");
        assert!(blob_row_exists(&b, h.as_str()).await, "{n}");
        assert!(b.blobs.get(&h).await.is_ok(), "{n}: the file of a referenced blob stays");
    })
    .await;
}

#[tokio::test]
async fn zero_since_follows_the_refcount_on_both_backends() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "zs").await;
        let m = mb.append(INBOX, common::raw("zs", None), Flags::default()).await.unwrap();
        assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE zero_since IS NOT NULL").await, 0, "{n}");
        mb.expunge_many_at(&[m.id], 100).await.unwrap();
        assert_eq!(mb.purge_deleted(i64::MAX).await.unwrap(), 1, "{n}");
        let lo = db_clock(&b).await;
        let z = scalar(&b, "SELECT zero_since FROM blobs").await;
        assert!((lo - 2..=lo + 2).contains(&z), "{n}: zero_since {z} is the DB clock {lo}");
        // Referencing it again clears it.
        mb.append(INBOX, common::raw("zs", None), Flags::default()).await.unwrap();
        assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE zero_since IS NOT NULL").await, 0, "{n}");
    })
    .await;
}

#[tokio::test]
async fn purge_deleted_keeps_young_and_delivered_messages_and_batches() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "pd").await;
        let gc = b.gc();
        let old = mb.append(INBOX, common::raw("old", None), Flags::default()).await.unwrap();
        let young = mb.append(INBOX, common::raw("young", None), Flags::default()).await.unwrap();
        let live = mb.append(INBOX, common::raw("live", None), Flags::default()).await.unwrap();
        let held = mb
            .deliver_at(INBOX, common::raw("held", None), Flags::default(), DeliveryId([9; 32]), 5)
            .await
            .unwrap();
        let held_id = match held {
            rusty_store::DeliverOutcome::Created(r) | rusty_store::DeliverOutcome::Duplicate(r) => r.id,
        };
        mb.expunge_many_at(&[old.id], 100).await.unwrap();
        mb.expunge_many_at(&[young.id], 5000).await.unwrap();
        mb.expunge_many_at(&[held_id], 100).await.unwrap();
        assert_eq!(gc.purge_deleted(1000).await.unwrap(), 1, "{n}: only the old, undelivered one");
        let _ = live;
        assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 3, "{n}");
        // After the delivery row is swept the held message goes too.
        assert_eq!(gc.purge_deliveries_at(1_000_000, 86_400).await.unwrap(), 1, "{n}");
        assert_eq!(gc.purge_deleted(1000).await.unwrap(), 1, "{n}");
        // Across mailboxes and more than one batch.
        let (dom, _, _) = acct(&b, "pd3").await;
        let mb2 = acct_in(&b, dom, "second").await;
        for (m, count) in [(&mb, 600usize), (&mb, 401), (&mb2, 5)] {
            let raws: Vec<_> = (0..count).map(|i| common::raw(&format!("bulk{count}-{i}"), None)).collect();
            let refs = m.append_many(INBOX, raws, Flags::default()).await.unwrap();
            for chunk in refs.chunks(500) {
                let ids: Vec<_> = chunk.iter().map(|r| r.id).collect();
                m.expunge_many_at(&ids, 1).await.unwrap();
            }
        }
        assert_eq!(gc.purge_deleted(1000).await.unwrap(), 1006, "{n}");
    })
    .await;
}

#[tokio::test]
async fn purge_deliveries_and_capabilities_are_swept_by_age() {
    for_each_backend(|b| async move {
        let n = b.name;
        let gc = b.gc();
        let (_, _, mb) = acct(&b, "sw").await;
        for (i, t) in [(1u8, 10i64), (2, 100_000)] {
            mb.deliver_at(INBOX, common::raw(&format!("d{i}"), None), Flags::default(), DeliveryId([i; 32]), t)
                .await
                .unwrap();
        }
        assert_eq!(gc.purge_deliveries(50_000).await.unwrap(), 1, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM deliveries").await, 1, "{n}");
        // The DB-clock default drops the old row (created_at 100000) but keeps a fresh one.
        mb.deliver(INBOX, common::raw("fresh", None), Flags::default(), DeliveryId([3; 32])).await.unwrap();
        assert_eq!(gc.purge_deliveries_default().await.unwrap(), 1, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM deliveries").await, 1, "{n} fresh row kept");
    })
    .await;
}

/// Review PG5 I4: the delete re-checks, under its lock, that each row is still unreferenced
/// AND past the cutoff. The listing step filters by the cutoff too, so the boundary has to be
/// driven at `delete_blob_rows` itself: a row at `cutoff` or after it stays, one before goes.
#[tokio::test]
async fn delete_blob_rows_rechecks_the_cutoff_itself() {
    for_each_backend(|b| async move {
        let n = b.name;
        let cutoff = 1_000_000i64;
        let before = blob(&b, "before", Some(cutoff - 1)).await;
        let at = blob(&b, "at", Some(cutoff)).await;
        let after = blob(&b, "after", Some(cutoff + 1)).await;
        let all = [at.clone(), after.clone(), before.clone()];
        let gone = b.grepo.delete_blob_rows(&all, cutoff).await.unwrap();
        assert_eq!(gone, vec![before.clone()], "{n}: only cutoff-1 is deleted");
        assert!(!blob_row_exists(&b, before.as_str()).await, "{n}");
        assert!(blob_row_exists(&b, at.as_str()).await, "{n}: zero_since = cutoff stays");
        assert!(blob_row_exists(&b, after.as_str()).await, "{n}: zero_since = cutoff+1 stays");
        // The same call one second later takes the boundary row.
        let gone = b.grepo.delete_blob_rows(&all, cutoff + 1).await.unwrap();
        assert_eq!(gone, vec![at.clone()], "{n}: the row at the old cutoff goes once past it");
        assert!(blob_row_exists(&b, after.as_str()).await, "{n}");
    })
    .await;
}

/// Review PG5 I4: a candidate listed while it was past the grace window, then referenced and
/// released again (which stamps a fresh `zero_since`), must NOT be deleted by the batch that
/// still holds the stale listing.
#[tokio::test]
async fn a_candidate_rereferenced_and_released_after_listing_is_not_deleted() {
    for_each_backend(|b| async move {
        let n = b.name;
        let now = db_clock(&b).await;
        let cutoff = now - 3600;
        let h = blob(&b, "relisted", Some(now - 7200)).await;
        let listed = b.grepo.blob_candidates(cutoff, None, 10).await.unwrap();
        assert_eq!(listed, vec![h.clone()], "{n}: listed as a candidate");
        // Another session appends a message with this content and expunges and purges it again.
        let lit = hash_lit(&b, h.as_str());
        run(&b, &format!("UPDATE blobs SET refcount = 1 WHERE hash = {lit}")).await;
        run(&b, &format!("UPDATE blobs SET refcount = 0 WHERE hash = {lit}")).await;
        let gone = b.grepo.delete_blob_rows(&listed, cutoff).await.unwrap();
        assert!(gone.is_empty(), "{n}: unreferenced for less than the grace window: {gone:?}");
        assert!(blob_row_exists(&b, h.as_str()).await, "{n}");
        // Still referenced: stays as well.
        run(&b, &format!("UPDATE blobs SET refcount = 1 WHERE hash = {lit}")).await;
        let gone = b.grepo.delete_blob_rows(&listed, i64::MAX).await.unwrap();
        assert!(gone.is_empty(), "{n}: referenced");
    })
    .await;
}
