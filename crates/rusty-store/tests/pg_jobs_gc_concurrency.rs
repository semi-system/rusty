//! PG5: real-concurrency behaviour of the Postgres job and GC repositories.
//! Requires `RUSTY_TEST_PG_URL` (loud SKIPPED notice otherwise).
#![cfg(feature = "postgres")]

mod common;

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use common::backends::{acct, pg_backend, scalar, Backend, INBOX};
use common::pg::pg_admin_url;
use common::raw;
use rusty_core::{Error, Flags, JobId};
use rusty_store::repo::pg::{Knobs, PgRepo};
use rusty_store::repo::{GcRepo, JobRepo};
use rusty_store::{Gc, JobFence, JobQueue, Mailbox, PgPoolConfig, PgStore};
use serde_json::json;

const HOUR: Duration = Duration::from_secs(3600);

fn repo_with(b: &Backend, k: Knobs) -> Arc<PgRepo> {
    let (store, _) = b.pg.as_ref().unwrap();
    Arc::new(PgRepo::new(store.clone()).with_knobs_for_test(k))
}

fn jq(r: &Arc<PgRepo>) -> JobQueue {
    JobQueue::with_repo(r.clone())
}

async fn open(b: &Backend, r: &Arc<PgRepo>, acc: rusty_core::AccountId) -> Mailbox {
    Mailbox::open_with(r.clone(), b.blobs.clone(), acc).await.unwrap()
}

/// Wait until some session of this database is inside `pg_sleep` (a knob pause): the
/// deterministic "the other session got there".
async fn wait_for_sleeper(pool: &sqlx::PgPool) {
    for _ in 0..1200 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() \
             AND state = 'active' AND query LIKE 'SELECT pg_sleep%'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if n > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no session ever slept");
}

async fn blob_hash_of(b: &Backend, bytes: bytes::Bytes) -> rusty_core::BlobHash {
    b.blobs.put(bytes).await.unwrap()
}

fn hex_lit(h: &rusty_core::BlobHash) -> String {
    format!("decode('{}', 'hex')", h.as_str())
}

// ---------------------------------------------------------------- claim

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixteen_claimers_claim_each_job_exactly_once_without_starvation() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let q = jq(&repo_with(&b, Knobs::default()));
    for i in 0..100 {
        q.enqueue("t", None, &json!({ "i": i }), None).await.unwrap();
    }
    let mut tasks = vec![];
    for w in 0..16 {
        let q = q.clone();
        tasks.push(tokio::spawn(async move {
            let mut got = vec![];
            // 16 x 6 = 96 <= 100: every single claim must find a job (no claimer is starved or
            // sent away empty-handed by another claimer's row lock).
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
            assert!(all.insert(id), "job {id:?} claimed twice");
        }
    }
    assert_eq!(all.len(), 96);
    assert_eq!(scalar(&b, "SELECT count(*) FROM jobs WHERE state = 'running' AND epoch = 1 AND attempts = 1").await, 96);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skip_locked_skips_a_held_job_and_does_not_skip_it_after_the_holder_rolls_back() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let (store, _) = b.pg.as_ref().unwrap();
    let q = jq(&repo_with(&b, Knobs::default()));
    let j1 = q.enqueue("t", None, &json!({}), Some(1)).await.unwrap();
    let j2 = q.enqueue("t", None, &json!({}), Some(2)).await.unwrap();
    // Another claimer is in the middle of claiming j1: its transaction holds the row lock.
    let mut holder = store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(j1.0)
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(3), q.claim("w", &["t"], 30))
        .await
        .expect("claim must not wait for a locked row")
        .unwrap()
        .expect("the free job is claimed");
    assert_eq!(got.id, j2);
    // The holder gives up: the job is claimable again (not lost, not skipped for good).
    holder.rollback().await.unwrap();
    let again = q.claim("w", &["t"], 30).await.unwrap().expect("j1 after rollback");
    assert_eq!(again.id, j1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_that_hits_lock_timeout_is_busy_not_storage() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let url = b._db.as_ref().unwrap().url().to_owned();
    let cfg = PgPoolConfig { lock_timeout: Duration::from_millis(300), ..PgPoolConfig::default() };
    let store = PgStore::connect_with_config(&url, cfg).await.unwrap();
    let r = Arc::new(PgRepo::new(store));
    let q = jq(&r);
    q.enqueue("t", None, &json!({}), None).await.unwrap();
    let (admin, _) = b.pg.as_ref().unwrap();
    let mut holder = admin.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE jobs IN ACCESS EXCLUSIVE MODE").execute(&mut *holder).await.unwrap();
    let err = q.claim("w", &["t"], 30).await.unwrap_err();
    assert!(matches!(err, Error::Busy(_)), "{err:?}");
    let err = q.enqueue("t", None, &json!({}), None).await.unwrap_err();
    assert!(matches!(err, Error::Busy(_)), "{err:?}");
    holder.rollback().await.unwrap();
    assert!(q.claim("w", &["t"], 30).await.unwrap().is_some());
}

/// A zombie heartbeat races the reaper's claim over a lapsed lease: the lapsed lease can never
/// be revived by the old epoch, whichever statement runs first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claim_versus_heartbeat_versus_reaper_race() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let q = jq(&repo_with(&b, Knobs::default()));
    for round in 0..30 {
        let id = q.enqueue(&format!("r{round}"), None, &json!({}), None).await.unwrap();
        let zombie = q.claim("w1", &[&format!("r{round}")], 60).await.unwrap().unwrap();
        sqlx::query("UPDATE jobs SET lease_expires_at = 1 WHERE id = $1")
            .bind(id.0)
            .execute(b.pg.as_ref().unwrap().0.pool())
            .await
            .unwrap();
        let (qa, qb, qc) = (q.clone(), q.clone(), q.clone());
        let kind = format!("r{round}");
        let hb = tokio::spawn(async move { qa.heartbeat(id, zombie.epoch, 60).await });
        let rc = tokio::spawn(async move { qb.claim("w2", &[&kind], 60).await });
        let cm = tokio::spawn(async move { qc.complete(id, zombie.epoch).await });
        assert!(matches!(hb.await.unwrap(), Err(Error::Conflict(_))), "round {round}: zombie heartbeat");
        assert!(matches!(cm.await.unwrap(), Err(Error::Conflict(_))), "round {round}: zombie complete");
        let owner = rc.await.unwrap().unwrap().expect("reaper claims the lapsed job");
        assert_eq!(owner.epoch, zombie.epoch + 1, "round {round}");
        assert_eq!(
            scalar(&b, &format!("SELECT epoch FROM jobs WHERE id = {}", id.0)).await,
            owner.epoch
        );
    }
}

// ---------------------------------------------------------------- enqueue cap

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_enqueues_at_the_cap_never_exceed_it() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let (store, _) = b.pg.as_ref().unwrap();
    // Each transaction stays open 100 ms after its insert, so an unserialised count-then-insert
    // would let several enqueues see the same count.
    let r = Arc::new(
        PgRepo::new(store.clone())
            .with_knobs_for_test(Knobs { pause_before_commit_ms: 100, ..Knobs::default() }),
    );
    let q = JobQueue::with_repo_and_limits(r.clone(), 3, 3);
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
                Err(e) => panic!("{e:?}"),
            }
        }
        assert_eq!((ok, full), (3, 5), "scope {scope:?}");
    }
}

// ---------------------------------------------------------------- retry exhaustion

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_forced_deadlocks_are_busy_for_claim_and_gc_and_change_nothing() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let r = repo_with(&b, Knobs::default());
    let q = jq(&r);
    let id = q.enqueue("t", None, &json!({}), None).await.unwrap();
    r.inject_deadlocks_for_test(5);
    let err = q.claim("w", &["t"], 30).await.unwrap_err();
    assert!(matches!(err, Error::Busy(_)), "{err:?}");
    assert_eq!(r.stats().exhausted.load(Ordering::Relaxed), 1);
    assert_eq!(scalar(&b, &format!("SELECT attempts + epoch FROM jobs WHERE id = {}", id.0)).await, 0);
    assert!(q.claim("w", &["t"], 30).await.unwrap().is_some(), "works afterwards");

    let h = blob_hash_of(&b, raw("orphan-row", None)).await;
    common::backends::insert_blob_row(&b, h.as_str(), 5).await;
    common::backends::set_zero_since(&b, h.as_str(), 0).await;
    r.inject_deadlocks_for_test(5);
    let err = GcRepo::delete_blob_rows(&*r, std::slice::from_ref(&h), i64::MAX).await.unwrap_err();
    assert!(matches!(err, Error::Busy(_)), "{err:?}");
    assert_eq!(r.stats().exhausted.load(Ordering::Relaxed), 2);
    assert!(common::backends::blob_row_exists(&b, h.as_str()).await, "nothing deleted");
    assert_eq!(GcRepo::delete_blob_rows(&*r, std::slice::from_ref(&h), i64::MAX).await.unwrap(), vec![h]);
}

// ---------------------------------------------------------------- GC versus references

/// GC deleted the row but has not committed; an append of the same blob arrives, waits for the
/// delete, then re-creates the row: one winner, no orphan message, refcount exactly 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_delete_in_flight_while_a_new_reference_arrives() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let signal = Arc::new(tokio::sync::Notify::new());
    let (store, _) = b.pg.as_ref().unwrap();
    let gc_repo = Arc::new(
        PgRepo::new(store.clone())
            .with_knobs_for_test(Knobs { pause_before_commit_ms: 800, ..Knobs::default() })
            .with_work_done_signal_for_test(signal.clone()),
    );
    let plain = repo_with(&b, Knobs::default());
    let (_, acc, _) = acct(&b, "inflight").await;
    let mb = open(&b, &plain, acc).await;
    let msg = raw("in-flight", None);
    let h = blob_hash_of(&b, msg.clone()).await;
    common::backends::insert_blob_row(&b, h.as_str(), msg.len() as i64).await;
    common::backends::set_zero_since(&b, h.as_str(), 0).await;
    let gc = Gc::new(gc_repo);
    let blobs = b.blobs.clone();
    let collector = tokio::spawn(async move { gc.collect_blobs_at(blobs.as_ref(), HOUR, SystemTime::now()).await });
    signal.notified().await; // the delete ran; its transaction (and lock) is still open
    let appended = mb.append(INBOX, msg.clone(), Flags::default()).await.expect("append must succeed");
    let rep = collector.await.unwrap().unwrap();
    assert_eq!(rep.rows_deleted, 1, "{rep:?}");
    assert_eq!(scalar(&b, &format!("SELECT refcount FROM blobs WHERE hash = {}", hex_lit(&h))).await, 1);
    assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE zero_since IS NOT NULL").await, 0);
    assert_eq!(raw_of(&b, &mb, appended.id).await, msg, "the message is readable: file kept");
}

/// An append holds the blob (row inserted, message in flight); the GC batch must wait for it and
/// then find the blob referenced: nothing deleted, no foreign-key error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_batch_waits_for_an_in_flight_reference_and_then_keeps_the_blob() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let signal = Arc::new(tokio::sync::Notify::new());
    let (store, _) = b.pg.as_ref().unwrap();
    let app_repo = Arc::new(
        PgRepo::new(store.clone())
            .with_knobs_for_test(Knobs { pause_before_commit_ms: 800, ..Knobs::default() })
            .with_work_done_signal_for_test(signal.clone()),
    );
    let plain = repo_with(&b, Knobs::default());
    let (_, acc, _) = acct(&b, "waits").await;
    let mb = open(&b, &app_repo, acc).await;
    let msg = raw("held", None);
    let h = blob_hash_of(&b, msg.clone()).await;
    common::backends::insert_blob_row(&b, h.as_str(), msg.len() as i64).await;
    common::backends::set_zero_since(&b, h.as_str(), 0).await;
    let appender = {
        let msg = msg.clone();
        tokio::spawn(async move { mb.append(INBOX, msg, Flags::default()).await })
    };
    signal.notified().await; // append holds the blob lock, uncommitted
    let gc = Gc::new(plain);
    let rep = gc.collect_blobs_at(b.blobs.as_ref(), HOUR, SystemTime::now()).await.unwrap();
    appender.await.unwrap().unwrap();
    assert_eq!(rep.rows_deleted, 0, "{rep:?}");
    assert!(rep.errors.is_empty(), "{rep:?}");
    assert_eq!(scalar(&b, &format!("SELECT refcount FROM blobs WHERE hash = {}", hex_lit(&h))).await, 1);
}

/// The interleaving "a delete commits between my blob insert and my lock": the append's INSERT
/// .. ON CONFLICT DO NOTHING skips the existing row, a GC deletes and commits, the append's lock
/// finds nothing and the bounded loop re-inserts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_delete_commits_between_my_blob_insert_and_my_lock() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let app_repo = repo_with(&b, Knobs { pause_after_blob_insert_ms: 1500, ..Knobs::default() });
    let plain = repo_with(&b, Knobs::default());
    let (_, acc, _) = acct(&b, "between").await;
    let mb = open(&b, &app_repo, acc).await;
    let msg = raw("between", None);
    let h = blob_hash_of(&b, msg.clone()).await;
    common::backends::insert_blob_row(&b, h.as_str(), msg.len() as i64).await;
    common::backends::set_zero_since(&b, h.as_str(), 0).await;
    let appender = {
        let (mb, msg) = (mb.clone(), msg.clone());
        tokio::spawn(async move { mb.append(INBOX, msg, Flags::default()).await })
    };
    wait_for_sleeper(b.pg.as_ref().unwrap().0.pool()).await; // insert done, lock not yet taken
    let rep = Gc::new(plain)
        .collect_blobs_at(b.blobs.as_ref(), HOUR, SystemTime::now())
        .await
        .unwrap();
    assert_eq!(rep.rows_deleted, 1, "the GC deleted the row in the window: {rep:?}");
    let appended = appender.await.unwrap().expect("the bounded loop re-inserts the row");
    assert_eq!(scalar(&b, &format!("SELECT refcount FROM blobs WHERE hash = {}", hex_lit(&h))).await, 1);
    assert_eq!(raw_of(&b, &mb, appended.id).await, msg);
}

// ---------------------------------------------------------------- GC versus purge_deleted ordering

/// A GC batch over blobs {A, B} and `purge_deleted` of messages referencing A and B. With a
/// crossed lock order (control) they deadlock for real and the retry loop recovers; with the
/// real ordering rule there is never a deadlock.
async fn gc_versus_purge(gc_knobs: Knobs, purge_knobs: Knobs) -> (Arc<PgRepo>, Arc<PgRepo>, usize) {
    let b = pg_backend().await.unwrap();
    let gc_repo = repo_with(&b, gc_knobs);
    let purge_repo = repo_with(&b, purge_knobs);
    let (_, acc, setup) = acct(&b, "gcpurge").await;
    let mb = open(&b, &purge_repo, acc).await;
    let (ma, mbb) = (raw("blob-a", None), raw("blob-b", None));
    let (ha, hb) = (blob_hash_of(&b, ma.clone()).await, blob_hash_of(&b, mbb.clone()).await);
    // The purge walks its messages by id: store the lower hash first so its order is ascending.
    let ((ma, ha), (mbb, hb)) =
        if ha.as_str() <= hb.as_str() { ((ma, ha), (mbb, hb)) } else { ((mbb, hb), (ma, ha)) };
    let r1= setup.append(INBOX, ma, Flags::default()).await.unwrap();
    let r2 = setup.append(INBOX, mbb, Flags::default()).await.unwrap();
    setup.expunge_many_at(&[r1.id, r2.id], 1).await.unwrap();
    let hashes = vec![ha, hb];
    let purge = tokio::spawn(async move { mb.purge_deleted(i64::MAX).await });
    wait_for_sleeper(b.pg.as_ref().unwrap().0.pool()).await; // purge holds its first blob lock
    let g = gc_repo.clone();
    let deleted = GcRepo::delete_blob_rows(&*g, &hashes, i64::MAX).await.expect("GC batch must succeed");
    assert_eq!(purge.await.unwrap().unwrap(), 2);
    (gc_repo, purge_repo, deleted.len())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crossed_lock_order_between_gc_and_purge_deadlocks_and_the_retry_recovers() {
    if pg_admin_url().is_none() {
        return;
    }
    let hold = |reverse| Knobs {
        unordered_blob_locks: true,
        reverse_blob_lock_order: reverse,
        hold_after_first_blob_ms: 600,
        ..Knobs::default()
    };
    let (gc, purge, _) = gc_versus_purge(hold(true), hold(false)).await;
    let d = gc.stats().deadlocks.load(Ordering::Relaxed) + purge.stats().deadlocks.load(Ordering::Relaxed);
    assert!(d >= 1, "the crossed order must really deadlock (control)");
    assert_eq!(gc.stats().exhausted.load(Ordering::Relaxed) + purge.stats().exhausted.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_gc_batch_orders_its_blob_locks_like_every_other_path() {
    if pg_admin_url().is_none() {
        return;
    }
    // The purge side is slowed down (pause after its first lock, ascending); the GC side uses the
    // production lock order: it must queue behind the purge, never deadlock with it.
    let slow_purge = Knobs { unordered_blob_locks: true, hold_after_first_blob_ms: 600, ..Knobs::default() };
    let (gc, purge, deleted) = gc_versus_purge(Knobs::default(), slow_purge).await;
    assert_eq!(deleted, 2, "the GC batch queued behind the purge and then found both rows unreferenced");
    assert_eq!(gc.stats().deadlocks.load(Ordering::Relaxed), 0);
    assert_eq!(gc.stats().retries.load(Ordering::Relaxed), 0);
    assert_eq!(purge.stats().deadlocks.load(Ordering::Relaxed), 0);
}

// ---------------------------------------------------------------- fenced mailbox operation races

/// A fenced delivery of a worker whose lease lapsed races the reaper's claim: the effect can
/// never commit under the old epoch once the new epoch exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fenced_delivery_of_a_zombie_races_the_reclaim() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let r = repo_with(&b, Knobs::default());
    let q = jq(&r);
    let (_, acc, _) = acct(&b, "zombie").await;
    let mb = open(&b, &r, acc).await;
    for round in 0u8..10 {
        let kind = format!("z{round}");
        let id: JobId = q.enqueue(&kind, None, &json!({}), None).await.unwrap();
        let zombie = q.claim("w1", &[&kind], 60).await.unwrap().unwrap();
        sqlx::query("UPDATE jobs SET lease_expires_at = 1 WHERE id = $1")
            .bind(id.0)
            .execute(b.pg.as_ref().unwrap().0.pool())
            .await
            .unwrap();
        let (m, qq, k) = (mb.clone(), q.clone(), kind.clone());
        let f = tokio::spawn(async move {
            m.deliver_fenced(INBOX, raw(&format!("zz{round}"), None), Flags::default(),
                rusty_core::DeliveryId([round + 1; 32]), JobFence { id, epoch: zombie.epoch }).await
        });
        let c = tokio::spawn(async move { qq.claim("w2", &[&k], 60).await });
        assert!(matches!(f.await.unwrap(), Err(Error::Conflict(_))), "round {round}");
        assert!(c.await.unwrap().unwrap().is_some(), "round {round}");
    }
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 0);
}

async fn raw_of(b: &Backend, mb: &Mailbox, id: rusty_core::MessageId) -> bytes::Bytes {
    let r = mb.get_raw_ref(id).await.unwrap();
    b.blobs.get(&r.blob).await.expect("the blob file of a referenced message exists")
}
