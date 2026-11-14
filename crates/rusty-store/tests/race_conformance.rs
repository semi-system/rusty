//! Races at the facade level (review PG6).
//!
//! The Postgres suites `pg_mailbox_concurrency.rs` and `pg_jobs_gc_concurrency.rs` force exact
//! interleavings with test knobs on row locks, which are internals of the repositories. The OUTCOME those
//! tests assert is what a caller observes: whichever of two racing operations runs first, the
//! other sees its committed result, and no orphan row, wrong refcount or cycle remains. Here the
//! same outcomes are asserted once for each serial order (deterministic)
//! and once with the operations really running at the same time (either serial outcome is fine).

mod common;

use std::collections::{HashMap, HashSet};

use common::backends::{acct, for_each_backend, scalar, Backend, INBOX};
use common::raw;
use rusty_core::{Error, Flags, FolderId};
use rusty_store::{ChangeOp, Mailbox, MAX_FOLDER_DEPTH};
use serde_json::json;

async fn parents(m: &Mailbox) -> HashMap<i64, Option<i64>> {
    m.list_folders()
        .await
        .unwrap()
        .into_iter()
        .map(|f| (f.id.0, f.parent.map(|p| p.0)))
        .collect()
}

fn depth_of(p: &HashMap<i64, Option<i64>>, mut id: i64) -> usize {
    let mut d = 1;
    while let Some(Some(parent)) = p.get(&id) {
        id = *parent;
        d += 1;
        assert!(d <= 1000, "a cycle");
    }
    d
}

async fn messages_of(b: &Backend, mb: &Mailbox) -> i64 {
    scalar(b, &format!("SELECT count(*) FROM messages WHERE mailbox_id = {}", mb.id().0)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_folder_versus_append_has_one_winner_and_no_orphan() {
    for_each_backend(|b| async move {
        let n = b.name;
        // Delete first: the append finds no folder, leaves nothing behind, counters roll back.
        let (_, _, mb) = acct(&b, "dfa1").await;
        let f = mb.create_folder(None, "Tmp").await.unwrap();
        mb.delete_folder(f).await.unwrap();
        let app = mb.append(f, raw("late", None), Flags::default()).await;
        assert!(matches!(app, Err(Error::NotFound(_))), "{n}: {app:?}");
        assert_eq!(messages_of(&b, &mb).await, 0, "{n}: no orphan message");
        assert_eq!(scalar(&b, "SELECT count(*) FROM blobs").await, 0, "{n}: the rolled back blob row is gone");
        let again = mb.append(INBOX, raw("after", None), Flags::default()).await.unwrap();
        assert_eq!((again.id.0, again.uid), (1, 1), "{n}: counters rolled back with the failed append");

        // Append first: the delete sees the message and refuses.
        let (dom, _, _) = acct(&b, "dfa2").await;
        let acc = b.dir.create_account("bob", dom, None, None).await.unwrap();
        let mb2 = Mailbox::open_with(b.mrepo.clone(), b.blobs.clone(), acc).await.unwrap();
        let f = mb2.create_folder(None, "Tmp").await.unwrap();
        mb2.append(f, raw("early", None), Flags::default()).await.unwrap();
        let del = mb2.delete_folder(f).await;
        assert!(
            matches!(&del, Err(Error::Conflict(m)) if m == "folder still contains messages"),
            "{n}: {del:?}"
        );
        assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE refcount <> 1").await, 0, "{n}: refcounts exact");

        // Really concurrent: exactly one serial outcome, never an orphan.
        let (_, _, mb3) = acct(&b, "dfa3").await;
        let f = mb3.create_folder(None, "Tmp").await.unwrap();
        let (m1, m2) = (mb3.clone(), mb3.clone());
        let t1 = tokio::spawn(async move { m1.delete_folder(f).await });
        let t2 = tokio::spawn(async move { m2.append(f, raw("race", None), Flags::default()).await });
        let (del, app) = (t1.await.unwrap(), t2.await.unwrap());
        match (&del, &app) {
            (Ok(()), Err(Error::NotFound(_))) => assert_eq!(messages_of(&b, &mb3).await, 0, "{n}: orphan"),
            (Err(Error::Conflict(_)), Ok(_)) => assert_eq!(messages_of(&b, &mb3).await, 1, "{n}"),
            other => panic!("{n}: not a serial outcome: {other:?}"),
        }
        assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE refcount < 0").await, 0, "{n}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_versus_expunge_of_the_same_message_serializes() {
    for_each_backend(|b| async move {
        let n = b.name;
        // Move first, expunge second: both succeed, the message ends up expunged in Sent.
        let (_, _, mb) = acct(&b, "mve1").await;
        let m = mb.append(INBOX, raw("m", None), Flags::default()).await.unwrap();
        assert_eq!(mb.move_message(m.id, FolderId(2)).await.unwrap().uid, 1);
        mb.expunge(m.id).await.unwrap();
        let d = mb.get_message(m.id).await.unwrap();
        assert!(d.deleted && d.folder == FolderId(2), "{n}");
        let ops: Vec<ChangeOp> = mb.changes_since(0, 100).await.unwrap().changes.iter().map(|c| c.op).collect();
        assert_eq!(ops, vec![ChangeOp::Create, ChangeOp::Update, ChangeOp::Destroy], "{n}");

        // Expunge first, move second: the move must not resurrect or move the expunged row.
        let (_, _, mb) = acct(&b, "mve2").await;
        let m = mb.append(INBOX, raw("m", None), Flags::default()).await.unwrap();
        mb.expunge(m.id).await.unwrap();
        let mv = mb.move_message(m.id, FolderId(2)).await;
        assert!(matches!(mv, Err(Error::NotFound(_))), "{n}: {mv:?}");
        let d = mb.get_message(m.id).await.unwrap();
        assert!(d.deleted && d.folder == INBOX, "{n}: the expunged message stayed where it was");
        let ops: Vec<ChangeOp> = mb.changes_since(0, 100).await.unwrap().changes.iter().map(|c| c.op).collect();
        assert_eq!(ops, vec![ChangeOp::Create, ChangeOp::Destroy], "{n}");

        // Really concurrent: one of the two serial outcomes.
        let (_, _, mb) = acct(&b, "mve3").await;
        let m = mb.append(INBOX, raw("m", None), Flags::default()).await.unwrap();
        let (a, c) = (mb.clone(), mb.clone());
        let id = m.id;
        let t1 = tokio::spawn(async move { a.move_message(id, FolderId(2)).await });
        let t2 = tokio::spawn(async move { c.expunge(id).await });
        let (mv, ex) = (t1.await.unwrap(), t2.await.unwrap());
        ex.unwrap();
        let d = mb.get_message(id).await.unwrap();
        assert!(d.deleted, "{n}");
        match mv {
            Ok(_) => assert_eq!(d.folder, FolderId(2), "{n}: moved, then expunged"),
            Err(Error::NotFound(_)) => assert_eq!(d.folder, INBOX, "{n}: expunged, then the move refused"),
            other => panic!("{n}: {other:?}"),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_deleted_versus_append_of_the_same_blob_leaves_exactly_one_reference() {
    for_each_backend(|b| async move {
        let n = b.name;
        for order in ["purge first", "append first", "concurrent"] {
            let (dom, _, x) = acct(&b, &format!("pva{}", order.len())).await;
            let acc = b.dir.create_account("bob", dom, None, None).await.unwrap();
            let y = Mailbox::open_with(b.mrepo.clone(), b.blobs.clone(), acc).await.unwrap();
            let content = format!("shared-blob-{order}");
            let m = x.append(INBOX, raw(&content, None), Flags::default()).await.unwrap();
            x.expunge_many_at(&[m.id], 100).await.unwrap();
            match order {
                "purge first" => {
                    assert_eq!(x.purge_deleted(i64::MAX).await.unwrap(), 1, "{n}");
                    y.append(INBOX, raw(&content, None), Flags::default()).await.unwrap();
                }
                "append first" => {
                    y.append(INBOX, raw(&content, None), Flags::default()).await.unwrap();
                    assert_eq!(x.purge_deleted(i64::MAX).await.unwrap(), 1, "{n}");
                }
                _ => {
                    let (x2, y2, c) = (x.clone(), y.clone(), raw(&content, None));
                    let t1 = tokio::spawn(async move { x2.purge_deleted(i64::MAX).await });
                    let t2 = tokio::spawn(async move { y2.append(INBOX, c, Flags::default()).await });
                    assert_eq!(t1.await.unwrap().unwrap(), 1, "{n}");
                    t2.await.unwrap().unwrap();
                }
            }
            let live = scalar(&b, &format!("SELECT count(*) FROM messages WHERE mailbox_id = {}", y.id().0)).await;
            assert_eq!(live, 1, "{n} {order}");
            // Exactly the blob of this content is referenced once and not marked for deletion.
            assert_eq!(
                scalar(&b, "SELECT count(*) FROM blobs WHERE refcount = 1 AND zero_since IS NULL").await,
                scalar(&b, "SELECT count(*) FROM blobs WHERE refcount > 0").await,
                "{n} {order}: a referenced blob is not scheduled for deletion"
            );
            assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE refcount < 0 OR refcount > 1").await, 0, "{n} {order}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folder_moves_never_commit_a_cycle_or_exceed_the_depth_limit() {
    for_each_backend(|b| async move {
        let n = b.name;
        // Swap: the second move sees the first commit and is a Conflict.
        let (_, _, mb) = acct(&b, "swap").await;
        let (f6, f7) = (mb.create_folder(None, "A").await.unwrap(), mb.create_folder(None, "B").await.unwrap());
        mb.move_folder(f6, Some(f7)).await.unwrap();
        let r2 = mb.move_folder(f7, Some(f6)).await;
        assert!(matches!(r2, Err(Error::Conflict(_))), "{n}: {r2:?}");
        let p = parents(&mb).await;
        assert_eq!((p[&f6.0], p[&f7.0]), (Some(f7.0), None), "{n}: no cycle");

        // The same swap really concurrent: exactly one wins, the other is refused.
        let (_, _, mb) = acct(&b, "swap2").await;
        let (f6, f7) = (mb.create_folder(None, "A").await.unwrap(), mb.create_folder(None, "B").await.unwrap());
        let (m1, m2) = (mb.clone(), mb.clone());
        let t1 = tokio::spawn(async move { m1.move_folder(f6, Some(f7)).await });
        let t2 = tokio::spawn(async move { m2.move_folder(f7, Some(f6)).await });
        let (r1, r2) = (t1.await.unwrap(), t2.await.unwrap());
        assert!(r1.is_ok() != r2.is_ok(), "{n}: exactly one swap wins: {r1:?} {r2:?}");
        let loser = if r1.is_ok() { &r2 } else { &r1 };
        assert!(matches!(loser, Err(Error::Conflict(_))), "{n}: {loser:?}");
        let p = parents(&mb).await;
        assert!(p[&f6.0] == Some(f7.0) || p[&f7.0] == Some(f6.0), "{n}");
        assert!(p[&f6.0].is_none() || p[&f7.0].is_none(), "{n}: no cycle");

        // Depth boundary: a 63-level chain; X under its end is level 64, Y under X would be 65.
        let (_, _, mb) = acct(&b, "depth").await;
        let mut end = mb.create_folder(None, "c0").await.unwrap();
        for i in 1..63 {
            end = mb.create_folder(Some(end), &format!("c{i}")).await.unwrap();
        }
        let x = mb.create_folder(None, "X").await.unwrap();
        let y = mb.create_folder(None, "Y").await.unwrap();
        mb.move_folder(x, Some(end)).await.unwrap();
        let r2 = mb.move_folder(y, Some(x)).await;
        assert!(matches!(r2, Err(Error::Invalid(_))), "{n}: {r2:?}");
        let p = parents(&mb).await;
        assert_eq!((p[&x.0], p[&y.0]), (Some(end.0), None), "{n}");

        // The same race concurrently: whatever order wins, no chain is deeper than the limit.
        let (_, _, mb) = acct(&b, "depth2").await;
        let mut end = mb.create_folder(None, "c0").await.unwrap();
        for i in 1..63 {
            end = mb.create_folder(Some(end), &format!("c{i}")).await.unwrap();
        }
        let x = mb.create_folder(None, "X").await.unwrap();
        let y = mb.create_folder(None, "Y").await.unwrap();
        let (m1, m2) = (mb.clone(), mb.clone());
        let t1 = tokio::spawn(async move { m1.move_folder(x, Some(end)).await });
        let t2 = tokio::spawn(async move { m2.move_folder(y, Some(x)).await });
        let _ = (t1.await.unwrap(), t2.await.unwrap());
        let p = parents(&mb).await;
        for id in p.keys() {
            assert!(depth_of(&p, *id) <= MAX_FOLDER_DEPTH, "{n}: folder {id} is too deep");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixteen_claimers_claim_each_job_exactly_once() {
    for_each_backend(|b| async move {
        let n = b.name;
        let q = b.jobs();
        for i in 0..100 {
            q.enqueue("t", None, &json!({ "i": i }), None).await.unwrap();
        }
        let mut tasks = vec![];
        for w in 0..16 {
            let q = q.clone();
            tasks.push(tokio::spawn(async move {
                let mut got = vec![];
                for _ in 0..6 {
                    let j = q.claim(&format!("w{w}"), &["t"], 60).await.unwrap().expect("a job is left");
                    got.push(j.id);
                }
                got
            }));
        }
        let mut all = HashSet::new();
        for t in tasks {
            for id in t.await.unwrap() {
                assert!(all.insert(id), "{n}: job {id:?} claimed twice");
            }
        }
        assert_eq!(all.len(), 96, "{n}");
        assert_eq!(
            scalar(&b, "SELECT count(*) FROM jobs WHERE state = 'running' AND epoch = 1 AND attempts = 1").await,
            96,
            "{n}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claim_heartbeat_and_complete_of_a_lapsed_lease_leave_one_owner() {
    for_each_backend(|b| async move {
        let n = b.name;
        let q = b.jobs();
        for round in 0..30 {
            let kind = format!("r{round}");
            let id = q.enqueue(&kind, None, &json!({}), None).await.unwrap();
            let zombie = q.claim("w1", &[&kind], 60).await.unwrap().unwrap();
            common::backends::run(&b, &format!("UPDATE jobs SET lease_expires_at = 1 WHERE id = {}", id.0)).await;
            let (qa, qb, qc, k) = (q.clone(), q.clone(), q.clone(), kind.clone());
            let hb = tokio::spawn(async move { qa.heartbeat(id, zombie.epoch, 60).await });
            let rc = tokio::spawn(async move { qb.claim("w2", &[&k], 60).await });
            let cm = tokio::spawn(async move { qc.complete(id, zombie.epoch).await });
            assert!(matches!(hb.await.unwrap(), Err(Error::Conflict(_))), "{n} round {round}: zombie heartbeat");
            assert!(matches!(cm.await.unwrap(), Err(Error::Conflict(_))), "{n} round {round}: zombie complete");
            let owner = rc.await.unwrap().unwrap().expect("the reaper claims the lapsed job");
            assert_eq!(owner.epoch, zombie.epoch + 1, "{n} round {round}");
            assert_eq!(scalar(&b, &format!("SELECT epoch FROM jobs WHERE id = {}", id.0)).await, owner.epoch, "{n}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fenced_delivery_of_a_zombie_never_commits_once_the_job_is_reclaimed() {
    for_each_backend(|b| async move {
        let n = b.name;
        let q = b.jobs();
        let (_, _, mb) = acct(&b, "zombie").await;
        for round in 0u8..10 {
            let kind = format!("z{round}");
            let id = q.enqueue(&kind, None, &json!({}), None).await.unwrap();
            let zombie = q.claim("w1", &[&kind], 60).await.unwrap().unwrap();
            common::backends::run(&b, &format!("UPDATE jobs SET lease_expires_at = 1 WHERE id = {}", id.0)).await;
            let (m, qq, k) = (mb.clone(), q.clone(), kind.clone());
            let f = tokio::spawn(async move {
                m.deliver_fenced(
                    INBOX,
                    raw(&format!("zz{round}"), None),
                    Flags::default(),
                    rusty_core::DeliveryId([round + 1; 32]),
                    rusty_store::JobFence { id, epoch: zombie.epoch },
                )
                .await
            });
            let c = tokio::spawn(async move { qq.claim("w2", &[&k], 60).await });
            assert!(matches!(f.await.unwrap(), Err(Error::Conflict(_))), "{n} round {round}");
            assert!(c.await.unwrap().unwrap().is_some(), "{n} round {round}");
        }
        assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 0, "{n}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gc_that_deleted_a_blob_row_does_not_lose_a_later_reference_to_the_same_content() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "gcnew").await;
        let m = mb.append(INBOX, raw("gc-me", None), Flags::default()).await.unwrap();
        mb.expunge_many_at(&[m.id], 100).await.unwrap();
        assert_eq!(mb.purge_deleted(i64::MAX).await.unwrap(), 1, "{n}");
        // The GC collects the unreferenced row (and its file) ...
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(10 * 86_400);
        let rep = b
            .gc()
            .collect_blobs_at(b.blobs.as_ref(), std::time::Duration::from_secs(3600), future)
            .await
            .unwrap();
        assert_eq!(rep.rows_deleted, 1, "{n}: {rep:?}");
        // ... and the same content stored afterwards re-creates both: one winner, readable message.
        let again = mb.append(INBOX, raw("gc-me", None), Flags::default()).await.unwrap();
        assert_eq!(scalar(&b, "SELECT refcount FROM blobs").await, 1, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE zero_since IS NOT NULL").await, 0, "{n}");
        let r = mb.get_raw_ref(again.id).await.unwrap();
        assert_eq!(b.blobs.get(&r.blob).await.expect("the file exists").as_ref(), raw("gc-me", None).as_ref(), "{n}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_list_member_adds_get_a_dense_per_list_sequence() {
    for_each_backend(|b| async move {
        let n = b.name;
        let dom = b.drepo.create_domain("lists.test").await.unwrap();
        let one = b.drepo.create_list("one", dom).await.unwrap();
        let two = b.drepo.create_list("two", dom).await.unwrap();
        let mut tasks = vec![];
        for i in 0..24 {
            let (repo, list) = (b.drepo.clone(), if i % 2 == 0 { one } else { two });
            tasks.push(tokio::spawn(async move {
                repo.add_list_member(list, None, Some(&format!("m{i}@outside.test")), 1000).await
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap_or_else(|e| panic!("{n}: {e:?}"));
        }
        for list in [one, two] {
            let seqs = common::backends::scalars(
                &b,
                &format!("SELECT seq FROM list_members WHERE list_id = {} ORDER BY seq", list.0),
            )
            .await;
            assert_eq!(seqs, (1..=12).collect::<Vec<i64>>(), "{n}: dense 1..n per list");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_enqueues_at_the_cap_never_exceed_it() {
    for_each_backend(|b| async move {
        let n = b.name;
        let q = b.jobs_with_limits(3, 3);
        let dom = b.dir.create_domain("capc.test").await.unwrap();
        for scope in [Some(dom), None] {
            let mut tasks = vec![];
            for _ in 0..8 {
                let q = q.clone();
                tasks.push(tokio::spawn(async move { q.enqueue("t", scope, &json!({}), None).await }));
            }
            let (mut ok, mut full) = (0, 0);
            for t in tasks {
                match t.await.unwrap() {
                    Ok(_) => ok += 1,
                    Err(Error::Quota(_)) => full += 1,
                    Err(e) => panic!("{n}: {e:?}"),
                }
            }
            assert_eq!((ok, full), (3, 5), "{n} scope {scope:?}");
        }
    })
    .await;
}
