//! PG2 spike: Postgres-only behaviour that SQLite cannot have (deadlocks between mailboxes,
//! retry classification). Requires `RUSTY_TEST_PG_URL` (loud SKIPPED notice otherwise).
#![cfg(feature = "postgres")]

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use common::pg::{pg_admin_url, PgTestDb};
use common::raw;
use common::spike::{acct, pg_backend, Backend, INBOX};
use rusty_core::{Error, Flags};
use rusty_store::map_db_error;
use rusty_store::repo::pg::{is_retryable, Knobs, PgRepo};
use rusty_store::repo::SpikeMailbox;
use sqlx::Executor;

/// Two mailboxes, each with a `SpikeMailbox` over a repo with the given knobs.
async fn two_mailboxes(knobs: Knobs) -> (Backend, SpikeMailbox, SpikeMailbox, Arc<PgRepo>) {
    let b = pg_backend().await.expect("RUSTY_TEST_PG_URL");
    let (store, _) = b.pg.as_ref().unwrap();
    let repo = Arc::new(PgRepo::new(store.clone()).with_knobs_for_test(knobs));
    let (_, acc1, _) = acct(&b, "one").await;
    let (_, acc2, _) = acct(&b, "two").await;
    let m1 = SpikeMailbox::open(repo.clone(), b.blobs.clone(), acc1).await.unwrap();
    let m2 = SpikeMailbox::open(repo.clone(), b.blobs.clone(), acc2).await.unwrap();
    (b, m1, m2, repo)
}

/// Mailbox 1 stores [A, B], mailbox 2 stores [B, A] at the same time (fresh blobs).
async fn crossed_batches(m1: &SpikeMailbox, m2: &SpikeMailbox, round: usize) {
    let a = raw(&format!("A{round}"), None);
    let b = raw(&format!("B{round}"), None);
    let (m1, m2) = (m1.clone(), m2.clone());
    let (a1, b1, a2, b2) = (a.clone(), b.clone(), a, b);
    let t1 = tokio::spawn(async move {
        m1.append_many(INBOX, vec![a1, b1], Flags::default()).await
    });
    let t2 = tokio::spawn(async move {
        m2.append_many(INBOX, vec![b2, a2], Flags::default()).await
    });
    t1.await.unwrap().expect("batch 1 must succeed");
    t2.await.unwrap().expect("batch 2 must succeed");
}

/// Negative control: with the ordering rule disabled and a pause after the first lock, the
/// crossed batches deadlock for real; the retry loop turns the 40P01 victim into a success.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unordered_blob_locks_deadlock_and_the_retry_loop_recovers() {
    if pg_admin_url().is_none() {
        return;
    }
    let (b, m1, m2, repo) = two_mailboxes(Knobs {
        unordered_blob_locks: true,
        hold_after_first_blob_ms: 300,
    })
    .await;
    let started = Instant::now();
    crossed_batches(&m1, &m2, 0).await;
    let took = started.elapsed();
    let deadlocks = repo.stats().deadlocks.load(Ordering::Relaxed);
    let retries = repo.stats().retries.load(Ordering::Relaxed);
    eprintln!("forced deadlock: deadlocks={deadlocks} retries={retries} elapsed={took:?}");
    assert!(deadlocks >= 1, "the interleaving must really deadlock (control)");
    assert_eq!(repo.stats().exhausted.load(Ordering::Relaxed), 0);
    // Both batches are stored exactly once after the retry.
    assert_eq!(m1.list_folder(INBOX, 0, 10).await.unwrap().len(), 2);
    assert_eq!(m2.list_folder(INBOX, 0, 10).await.unwrap().len(), 2);
    drop(b);
}

/// The real rule: blobs locked in ascending hash order, so the same crossed workload never
/// deadlocks and never retries. Repeated to make an accidental pass unlikely.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_blob_locks_never_deadlock() {
    if pg_admin_url().is_none() {
        return;
    }
    let (b, m1, m2, repo) = two_mailboxes(Knobs::default()).await;
    for round in 0..40 {
        crossed_batches(&m1, &m2, round).await;
    }
    assert_eq!(repo.stats().retries.load(Ordering::Relaxed), 0);
    assert_eq!(repo.stats().deadlocks.load(Ordering::Relaxed), 0);
    assert_eq!(m1.list_folder(INBOX, 0, 1000).await.unwrap().len(), 80);
    drop(b);
}

/// Many mailboxes storing the SAME three blobs in random-ish orders, concurrently: the hot
/// shared blobs serialize the transactions at the `blobs` rows but never deadlock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_hot_blobs_across_many_mailboxes_do_not_deadlock() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let (store, _) = b.pg.as_ref().unwrap();
    let repo = Arc::new(PgRepo::new(store.clone()));
    let mut boxes = vec![];
    for i in 0..12 {
        let (_, acc, _) = acct(&b, &format!("hot{i}")).await;
        boxes.push(SpikeMailbox::open(repo.clone(), b.blobs.clone(), acc).await.unwrap());
    }
    let docs = [raw("hot-a", None), raw("hot-b", None), raw("hot-c", None)];
    let mut tasks = vec![];
    for (i, mb) in boxes.iter().enumerate() {
        let mb = mb.clone();
        let mut batch = docs.to_vec();
        batch.rotate_left(i % 3);
        if i % 2 == 1 {
            batch.reverse();
        }
        tasks.push(tokio::spawn(async move {
            for _ in 0..5 {
                mb.append_many(INBOX, batch.clone(), Flags::default()).await.unwrap();
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(repo.stats().deadlocks.load(Ordering::Relaxed), 0);
    let refcount: i64 = sqlx::query_scalar("SELECT max(refcount) FROM blobs")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(refcount, 12 * 5, "every message reference counted exactly once");
}

/// SQLSTATE classification, from real server errors: a deadlock is 40P01, a lost update under
/// REPEATABLE READ is 40001; both are retryable; neither leaks text through `map_db_error`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retryable_sqlstates_are_classified_from_real_errors() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create(&url).await;
    let store = db.connect().await;
    let pool = store.pool();
    sqlx::query("CREATE TABLE t (id int PRIMARY KEY, v int)").execute(pool).await.unwrap();
    sqlx::query("INSERT INTO t VALUES (1,0),(2,0)").execute(pool).await.unwrap();

    // 40001: REPEATABLE READ sees a concurrent committed update of the row it wants to change.
    let mut a = pool.begin().await.unwrap();
    a.execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ").await.unwrap();
    sqlx::query("SELECT v FROM t WHERE id = 1").fetch_one(&mut *a).await.unwrap();
    sqlx::query("UPDATE t SET v = 1 WHERE id = 1").execute(pool).await.unwrap();
    let e = sqlx::query("UPDATE t SET v = 2 WHERE id = 1").execute(&mut *a).await.unwrap_err();
    assert!(is_retryable(&e), "{e}");
    assert_eq!(e.as_database_error().unwrap().code().as_deref(), Some("40001"));
    let _ = a.rollback().await;

    // 40P01: two sessions lock rows 1 and 2 in opposite order.
    let mut x = pool.begin().await.unwrap();
    let mut y = pool.begin().await.unwrap();
    sqlx::query("UPDATE t SET v = 10 WHERE id = 1").execute(&mut *x).await.unwrap();
    sqlx::query("UPDATE t SET v = 20 WHERE id = 2").execute(&mut *y).await.unwrap();
    let fx = async { sqlx::query("UPDATE t SET v = 11 WHERE id = 2").execute(&mut *x).await };
    let fy = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        sqlx::query("UPDATE t SET v = 21 WHERE id = 1").execute(&mut *y).await
    };
    let (rx, ry) = tokio::join!(fx, fy);
    let errs: Vec<sqlx::Error> = [rx, ry].into_iter().filter_map(Result::err).collect();
    assert_eq!(errs.len(), 1, "exactly one deadlock victim");
    assert_eq!(errs[0].as_database_error().unwrap().code().as_deref(), Some("40P01"));
    assert!(is_retryable(&errs[0]));
    // Plain mapping (outside the retry loop) is a fixed Storage error, no server text.
    let mapped = map_db_error(errs.into_iter().next().unwrap());
    assert!(matches!(&mapped, Error::Storage { message, .. } if message == "database error"));
    let _ = x.rollback().await;
    let _ = y.rollback().await;
    // A unique violation is NOT retryable.
    let e = sqlx::query("INSERT INTO t VALUES (1, 1)").execute(pool).await.unwrap_err();
    assert!(!is_retryable(&e));
    store.close().await;
    db.cleanup().await;
}
