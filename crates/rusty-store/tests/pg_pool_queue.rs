#![cfg(feature = "postgres")]
//! The per-mailbox in-process write queue and the pool policy (U11, PG8 decisions 1 and 2).

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::pg::{pg_admin_url, PgTestDb};
use rusty_core::{AccountId, Error, MailboxId};
use rusty_store::repo::pg::{Knobs, PgRepo};
use rusty_store::repo::{DirectoryRepo, MailboxRepo, NewAccount};
use rusty_store::{MailboxQueue, PgPoolConfig, PgStore, STRIPES};

async fn account(repo: &PgRepo, tag: &str) -> (AccountId, MailboxId) {
    let dom = repo.create_domain(&format!("{tag}.test")).await.unwrap();
    let acc = repo
        .create_account(&NewAccount { local: "alice".into(), domain: dom, plan: None, password_hash: None })
        .await
        .unwrap();
    (acc, repo.mailbox_for_account(acc).await.unwrap())
}

// backend-neutral: the in-process queue sits in front of the Postgres pool; SQLite has one writer lock and no pool
#[tokio::test]
async fn permits_are_exclusive_fifo_and_released_on_drop() {
    let q = MailboxQueue::new(Duration::from_secs(5));
    let first = q.acquire(7).await.unwrap();
    assert_eq!(q.held(), 1);
    let q = Arc::new(q);
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut waiters = Vec::new();
    for i in 0..3 {
        let (q, order) = (q.clone(), order.clone());
        waiters.push(tokio::spawn(async move {
            let _p = q.acquire(7).await.unwrap();
            order.lock().unwrap().push(i);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }));
        // Queue them in a known order.
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(order.lock().unwrap().is_empty(), "nobody gets in while the permit is held");
    drop(first);
    for w in waiters {
        w.await.unwrap();
    }
    assert_eq!(*order.lock().unwrap(), vec![0, 1, 2], "FIFO");
    assert_eq!(q.held(), 0);
}

// backend-neutral: the in-process queue sits in front of the Postgres pool; SQLite has one writer lock and no pool
#[tokio::test]
async fn a_cancelled_waiter_leaks_nothing_and_a_deadline_is_busy() {
    let q = Arc::new(MailboxQueue::new(Duration::from_millis(150)));
    let held = q.acquire(1).await.unwrap();
    // Cancelled while waiting: its place disappears, the next waiter is not blocked by a ghost.
    let q2 = q.clone();
    let w = tokio::spawn(async move { q2.acquire(1).await.map(|_| ()) });
    tokio::time::sleep(Duration::from_millis(30)).await;
    w.abort();
    let _ = w.await;
    // Deadline: the wait is bounded and the answer is the retryable, fixed Busy.
    let started = Instant::now();
    let err = q.acquire(1).await.unwrap_err();
    assert!(started.elapsed() >= Duration::from_millis(120) && started.elapsed() < Duration::from_secs(2));
    match &err {
        Error::Busy(m) => assert!(m.len() < 40 && !m.contains('1'), "{m}"),
        other => panic!("{other:?}"),
    }
    drop(held);
    // After release a new writer gets in at once.
    let started = Instant::now();
    let _p = q.acquire(1).await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(100));
}

// backend-neutral: the in-process queue sits in front of the Postgres pool; SQLite has one writer lock and no pool
#[tokio::test]
async fn memory_is_bounded_whatever_the_mailbox_ids() {
    let q = MailboxQueue::new(Duration::from_secs(1));
    // 10,000 distinct ids (and hostile ones) leave no state behind: the table is a fixed array.
    for id in (0..10_000i64).chain([i64::MIN, i64::MAX, -1, 0]) {
        let p = q.acquire(id).await.unwrap();
        drop(p);
    }
    assert_eq!(q.held(), 0);
    assert!(MailboxQueue::stripe_of(i64::MIN) < STRIPES && MailboxQueue::stripe_of(i64::MAX) < STRIPES);
    // Sequential ids spread over the stripes (no pathological clustering).
    let mut seen = std::collections::HashSet::new();
    for id in 0..512i64 {
        seen.insert(MailboxQueue::stripe_of(id));
    }
    assert!(seen.len() > 400, "only {} distinct stripes for 512 sequential ids", seen.len());
}

// backend-neutral: the pool and the in-process queue exist only in the Postgres backend
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hot_mailbox_does_not_starve_a_cold_one_with_a_small_pool() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = PgStore::connect_with_config(
        db.url(),
        PgPoolConfig { max_connections: 2, acquire_timeout: Duration::from_secs(10), ..PgPoolConfig::default() },
    )
    .await
    .unwrap();
    let setup = PgRepo::new(store.clone());
    let (_, hot) = account(&setup, "hot").await;
    let (_, cold) = account(&setup, "cold").await;
    // Every transaction of this repository stays open 400 ms before COMMIT: the hot mailbox's
    // writers are slow, and there are many of them.
    let slow = PgRepo::new(store.clone())
        .with_knobs_for_test(Knobs { pause_before_commit_ms: 400, ..Knobs::default() });
    let fast = PgRepo::new(store.clone());
    let mut writers = Vec::new();
    for i in 0..6 {
        let slow = slow.clone();
        writers.push(tokio::spawn(async move { slow.create_folder(hot, None, &format!("hot-{i}")).await }));
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    // With the queue, only ONE hot writer holds a connection (the others wait in process), so a
    // connection is free for the cold mailbox. Without it two hot writers pin both connections.
    let started = Instant::now();
    fast.create_folder(cold, None, "cold-1").await.unwrap();
    let took = started.elapsed();
    assert!(took < Duration::from_millis(300), "the cold mailbox waited {took:?} behind the hot one");
    for w in writers {
        w.await.unwrap().unwrap();
    }
    db.cleanup().await;
}

// backend-neutral: the pool and the in-process queue exist only in the Postgres backend
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saturated_pool_and_a_saturated_queue_answer_busy_not_a_hang() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let cfg = PgPoolConfig {
        max_connections: 1,
        acquire_timeout: Duration::from_millis(300),
        queue_timeout: Duration::from_millis(300),
        ..PgPoolConfig::default()
    };
    let store = PgStore::connect_with_config(db.url(), cfg).await.unwrap();
    let repo = PgRepo::new(store.clone());
    let (_, mb) = account(&repo, "sat").await;

    // Pool saturated by somebody else: the write is Busy after the acquire timeout.
    let held = store.pool().acquire().await.unwrap();
    let started = Instant::now();
    let err = repo.create_folder(mb, None, "x").await.unwrap_err();
    assert!(matches!(err, Error::Busy(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
    // So is a read.
    let started = Instant::now();
    let err = repo.mailbox_for_account(AccountId(1)).await.unwrap_err();
    assert!(matches!(err, Error::Busy(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
    drop(held);
    repo.create_folder(mb, None, "x").await.unwrap();

    // Queue saturated: a slow writer holds the mailbox's turn, the next one is Busy after queue_timeout.
    let slow = PgRepo::new(store.clone())
        .with_knobs_for_test(Knobs { pause_before_commit_ms: 900, ..Knobs::default() });
    let w = tokio::spawn(async move { slow.create_folder(mb, None, "slow").await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let started = Instant::now();
    let err = repo.create_folder(mb, None, "y").await.unwrap_err();
    assert!(matches!(&err, Error::Busy(m) if m.contains("queue")), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    w.await.unwrap().unwrap();
    repo.create_folder(mb, None, "y").await.unwrap();
    db.cleanup().await;
}

// backend-neutral: the pool and the in-process queue exist only in the Postgres backend
#[tokio::test]
async fn pool_validation_rejects_zero_limits() {
    for cfg in [
        PgPoolConfig { max_connections: 0, ..PgPoolConfig::default() },
        PgPoolConfig { queue_timeout: Duration::ZERO, ..PgPoolConfig::default() },
        PgPoolConfig { acquire_timeout: Duration::ZERO, ..PgPoolConfig::default() },
    ] {
        let r = PgStore::connect_with_config("postgres://u@127.0.0.1:1/d?sslmode=disable", cfg).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    }
}
