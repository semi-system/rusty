//! PG2 spike: Postgres-only behaviour that SQLite cannot have (deadlocks between mailboxes,
//! retry classification). Requires `RUSTY_TEST_PG_URL` (loud SKIPPED notice otherwise).
#![cfg(feature = "postgres")]

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use common::pg::{pg_admin_url, PgTestDb};
use common::raw;
use common::backends::{acct, pg_backend, Backend, INBOX};
use rusty_core::{Error, Flags};
use rusty_store::map_db_error;
use rusty_store::repo::pg::{is_retryable, Knobs, PgRepo};
use rusty_store::Mailbox;
use sqlx::Executor;

/// Block until some session of this database waits for a heavyweight lock (row lock, transaction
/// id): the deterministic replacement for "sleep and hope the other session got there".
async fn wait_for_lock_waiter(pool: &sqlx::PgPool) {
    for _ in 0..1200 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \n             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if n > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no session ever waited for a lock");
}

/// Two mailboxes, each with a `Mailbox` over a repo with the given knobs.
async fn two_mailboxes(knobs: Knobs) -> (Backend, Mailbox, Mailbox, Arc<PgRepo>) {
    let b = pg_backend().await.expect("RUSTY_TEST_PG_URL");
    let (store, _) = b.pg.as_ref().unwrap();
    let repo = Arc::new(PgRepo::new(store.clone()).with_knobs_for_test(knobs));
    let (_, acc1, _) = acct(&b, "one").await;
    let (_, acc2, _) = acct(&b, "two").await;
    let m1 = Mailbox::open_with(repo.clone(), b.blobs.clone(), acc1).await.unwrap();
    let m2 = Mailbox::open_with(repo.clone(), b.blobs.clone(), acc2).await.unwrap();
    (b, m1, m2, repo)
}

/// Mailbox 1 stores [A, B], mailbox 2 stores [B, A] at the same time (fresh blobs).
async fn crossed_batches(m1: &Mailbox, m2: &Mailbox, round: usize) {
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
        ..Knobs::default()
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
        boxes.push(Mailbox::open_with(repo.clone(), b.blobs.clone(), acc).await.unwrap());
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
        wait_for_lock_waiter(pool).await;
        sqlx::query("UPDATE t SET v = 21 WHERE id = 1").execute(&mut *y).await
    };
    let (rx, ry) = tokio::join!(fx, fy);
    let errs: Vec<sqlx::Error> = [rx, ry].into_iter().filter_map(Result::err).collect();
    assert_eq!(errs.len(), 1, "exactly one deadlock victim");
    assert_eq!(errs[0].as_database_error().unwrap().code().as_deref(), Some("40P01"));
    assert!(is_retryable(&errs[0]));
    // Plain mapping (outside the retry loop) is the transient class (D1), fixed text, no server text.
    let mapped = map_db_error(errs.into_iter().next().unwrap());
    assert!(matches!(&mapped, Error::Busy(message) if message == "transaction conflict"));
    let _ = x.rollback().await;
    let _ = y.rollback().await;
    // A unique violation is NOT retryable.
    let e = sqlx::query("INSERT INTO t VALUES (1, 1)").execute(pool).await.unwrap_err();
    assert!(!is_retryable(&e));
    store.close().await;
    db.cleanup().await;
}

/// GC deleting an unreferenced blob row (uncommitted) while a new message of the same content
/// is being stored: the store waits for the delete, then re-creates the row and succeeds; the
/// message never ends up pointing at a deleted blob. (The other interleaving, delete committed
/// between the blob insert and the blob lock, is covered by the bounded loop in `lock_blobs`
/// and is NOT exercised here: it needs a pause between two statements of one transaction.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_delete_in_flight_does_not_lose_a_new_reference() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let (store, _) = b.pg.as_ref().unwrap();
    let (_, _, mb) = acct(&b, "gc").await;
    mb.append(INBOX, raw("gc-me", None), Flags::default()).await.unwrap();
    sqlx::query("DELETE FROM messages").execute(store.pool()).await.unwrap();
    let zero: i64 = sqlx::query_scalar("SELECT count(*) FROM blobs WHERE refcount = 0 AND zero_since IS NOT NULL")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(zero, 1);

    let mut gc = store.pool().begin().await.unwrap();
    sqlx::query("DELETE FROM blobs WHERE refcount = 0").execute(&mut *gc).await.unwrap();
    let mb2 = mb.clone();
    let task = tokio::spawn(async move {
        mb2.append(INBOX, raw("gc-me", None), Flags::default()).await
    });
    wait_for_lock_waiter(store.pool()).await;
    assert!(!task.is_finished(), "the append must wait for the in-flight delete");
    gc.commit().await.unwrap();
    task.await.unwrap().expect("append succeeds after the delete committed");
    let (rows, refcount): (i64, i64) = sqlx::query_as("SELECT count(*), max(refcount) FROM blobs")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!((rows, refcount), (1, 1));
}

// ====================================================================== PG4: mailbox concurrency
//
// Two-session interleavings are made deterministic with test knobs, without guessing delays:
// `pause_before_commit_ms` keeps the first session's transaction open (locks held) after its work
// and the repository signals (`with_work_done_signal_for_test`) at that moment; the second session
// is started on the signal, so it can only queue behind the first, never win the race.

use std::future::Future;
use std::time::Duration;

use common::backends::{scalar, scalars};
use bytes::Bytes;
use rusty_core::{AccountId, DeliveryId, DomainId, FolderId};
use rusty_store::{ChangeOp, DeliverItem, DeliverOutcome};

/// How long the first session keeps its locks after its work. Not a synchronisation delay: the
/// second session starts on the signal; this only has to be long enough for it to reach the lock.
const HOLD_MS: u32 = 500;
/// Upper bound for the signal; a missing signal fails the test instead of hanging it.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(60);

fn store_of(b: &Backend) -> rusty_store::PgStore {
    b.pg.as_ref().expect("postgres backend").0.clone()
}

fn repo_with(b: &Backend, knobs: Knobs) -> Arc<PgRepo> {
    Arc::new(PgRepo::new(store_of(b)).with_knobs_for_test(knobs))
}

/// A repository whose operations keep their transaction open for `HOLD_MS` before COMMIT and
/// signal the returned `Notify` when their work is done and the locks are held.
fn slow_repo(b: &Backend) -> (Arc<PgRepo>, Arc<tokio::sync::Notify>) {
    let held = Arc::new(tokio::sync::Notify::new());
    let repo = PgRepo::new(store_of(b))
        .with_knobs_for_test(Knobs {
            pause_before_commit_ms: HOLD_MS,
            ..Knobs::default()
        })
        .with_work_done_signal_for_test(held.clone());
    (Arc::new(repo), held)
}

async fn slow_session(b: &Backend, acc: AccountId) -> (Mailbox, Arc<tokio::sync::Notify>) {
    let (repo, held) = slow_repo(b);
    (open_on(b, &repo, acc).await, held)
}

async fn open_on(b: &Backend, repo: &Arc<PgRepo>, acc: AccountId) -> Mailbox {
    Mailbox::open_with(repo.clone(), b.blobs.clone(), acc).await.unwrap()
}

async fn new_account(b: &Backend, dom: DomainId, local: &str) -> AccountId {
    b.dir.create_account(local, dom, None, None).await.unwrap()
}

/// Run `first` in the background, start `second` once `first` signals that it holds its locks,
/// return both results.
async fn staggered<T1, T2>(
    held: Arc<tokio::sync::Notify>,
    first: impl Future<Output = T1> + Send + 'static,
    second: impl Future<Output = T2>,
) -> (T1, T2)
where
    T1: Send + 'static,
{
    let h = tokio::spawn(first);
    tokio::time::timeout(SIGNAL_TIMEOUT, held.notified())
        .await
        .expect("the first session never signalled that it holds its locks");
    let r2 = second.await;
    (h.await.unwrap(), r2)
}

fn did(n: u8) -> DeliveryId {
    DeliveryId([n; 32])
}

fn item(subject: &str, n: u8) -> DeliverItem {
    DeliverItem {
        raw: raw(subject, None),
        flags: Flags::default(),
        delivery_id: did(n),
    }
}

async fn parents(m: &Mailbox) -> std::collections::HashMap<i64, Option<i64>> {
    m.list_folders()
        .await
        .unwrap()
        .into_iter()
        .map(|f| (f.id.0, f.parent.map(|p| p.0)))
        .collect()
}

// ---------------------------------------------------------------- folder tree races (finding 1)

/// Two sessions move folders 6 -> under 7 and 7 -> under 6. With the mailbox lock first, the
/// second session waits, sees the first commit and is `Conflict`; no cycle is ever committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folder_swap_race_second_session_sees_the_first_commit() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let (_, acc, setup) = acct(&b, "swap").await;
    let (f6, f7) = (
        setup.create_folder(None, "A").await.unwrap(),
        setup.create_folder(None, "B").await.unwrap(),
    );
    let (slow, held) = slow_session(&b, acc).await;
    let (r1, r2) = staggered(held, async move { slow.move_folder(f6, Some(f7)).await }, setup.move_folder(f7, Some(f6))).await;
    r1.unwrap();
    assert!(matches!(r2, Err(Error::Conflict(_))), "{r2:?}");
    let p = parents(&setup).await;
    assert_eq!((p[&f6.0], p[&f7.0]), (Some(f7.0), None), "no cycle");
}

/// The same race with the repository lock SKIPPED: the BEFORE UPDATE trigger (which takes the
/// mailbox lock itself and walks the tree with a fresh statement snapshot) is the second line of
/// defence and rejects the cycle with RS002 (`Invalid`). Removing the trigger's `FOR UPDATE`
/// commits the cycle (mutation M-B).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folder_cycle_trigger_is_the_second_line_of_defence() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let (_, acc, setup) = acct(&b, "swap2").await;
    let (f6, f7) = (
        setup.create_folder(None, "A").await.unwrap(),
        setup.create_folder(None, "B").await.unwrap(),
    );
    let (slow, held) = slow_session(&b, acc).await;
    let unlocked = open_on(
        &b,
        &repo_with(&b, Knobs { skip_mailbox_lock: true, ..Knobs::default() }),
        acc,
    )
    .await;
    let (r1, r2) = staggered(held, async move { slow.move_folder(f6, Some(f7)).await }, unlocked.move_folder(f7, Some(f6))).await;
    r1.unwrap();
    assert!(matches!(r2, Err(Error::Invalid(_))), "{r2:?}");
    let p = parents(&setup).await;
    assert_eq!((p[&f6.0], p[&f7.0]), (Some(f7.0), None), "no cycle");
}

/// Depth boundary: a 63-level chain; session one moves X under its end (level 64, allowed),
/// session two moves Y under X. Serialized, the second sees level 64 + 1 and is `Invalid`; with
/// the repository lock skipped the trigger rejects it (also `Invalid`). Either way Y stays a root.
async fn depth_race(skip_lock: bool) {
    let b = pg_backend().await.unwrap();
    let (_, acc, setup) = acct(&b, if skip_lock { "dp2" } else { "dp1" }).await;
    let mut end = setup.create_folder(None, "c0").await.unwrap();
    for i in 1..63 {
        end = setup.create_folder(Some(end), &format!("c{i}")).await.unwrap();
    }
    let x = setup.create_folder(None, "X").await.unwrap();
    let y = setup.create_folder(None, "Y").await.unwrap();
    let (slow, held) = slow_session(&b, acc).await;
    let second = open_on(
        &b,
        &repo_with(&b, Knobs { skip_mailbox_lock: skip_lock, ..Knobs::default() }),
        acc,
    )
    .await;
    let (r1, r2) = staggered(held, async move { slow.move_folder(x, Some(end)).await }, second.move_folder(y, Some(x))).await;
    r1.unwrap();
    assert!(matches!(r2, Err(Error::Invalid(_))), "skip_lock={skip_lock}: {r2:?}");
    let p = parents(&setup).await;
    assert_eq!((p[&x.0], p[&y.0]), (Some(end.0), None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn depth_boundary_race_is_serialized_by_the_mailbox_lock() {
    if pg_admin_url().is_none() {
        return;
    }
    depth_race(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn depth_boundary_race_is_rejected_by_the_trigger_without_the_lock() {
    if pg_admin_url().is_none() {
        return;
    }
    depth_race(true).await;
}

// ---------------------------------------------------------------- delete_folder, move, purge

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_folder_versus_append_has_one_winner_and_no_orphan() {
    if pg_admin_url().is_none() {
        return;
    }
    // Order 1: the delete holds its locks; the append queues behind it and finds no folder.
    let b = pg_backend().await.unwrap();
    let (_, acc, fast) = acct(&b, "dfa1").await;
    let f = fast.create_folder(None, "Tmp").await.unwrap();
    let (slow, held) = slow_session(&b, acc).await;
    let (del, app) = staggered(held, async move { slow.delete_folder(f).await }, fast.append(f, raw("late", None), Flags::default())).await;
    del.unwrap();
    assert!(matches!(app, Err(Error::NotFound(_))), "{app:?}");
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 0, "no orphan message");
    assert_eq!(scalar(&b, "SELECT count(*) FROM blobs").await, 0, "the rolled back blob row is gone");
    let again = fast.append(FolderId(1), raw("after", None), Flags::default()).await.unwrap();
    assert_eq!((again.id.0, again.uid), (1, 1), "counters rolled back with the failed append");

    // Order 2: the append holds its locks; the delete queues behind it and sees the message.
    let (_, acc, fast) = acct(&b, "dfa2").await;
    let f = fast.create_folder(None, "Tmp").await.unwrap();
    let (slow, held) = slow_session(&b, acc).await;
    let (app, del) = staggered(held, async move { slow.append(f, raw("early", None), Flags::default()).await }, fast.delete_folder(f)).await;
    app.unwrap();
    // The exact message proves the repository check saw the committed message; the foreign key
    // would also stop the delete (as a different, generic Conflict).
    assert!(
        matches!(&del, Err(Error::Conflict(m)) if m == "folder still contains messages"),
        "{del:?}"
    );
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages WHERE folder_id > 5").await, 1);
    assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE refcount <> 1").await, 0, "no negative or doubled refcount");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_versus_expunge_of_the_same_message_serializes() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    // Order 1: move first, expunge second: both succeed, the message ends up expunged in Sent.
    let (_, acc, fast) = acct(&b, "mve1").await;
    let m = fast.append(INBOX, raw("m", None), Flags::default()).await.unwrap();
    let (slow, held) = slow_session(&b, acc).await;
    let (mv, ex) = staggered(held, async move { slow.move_message(m.id, FolderId(2)).await }, fast.expunge(m.id)).await;
    assert_eq!(mv.unwrap().uid, 1);
    ex.unwrap();
    let d = fast.get_message(m.id).await.unwrap();
    assert!(d.deleted && d.folder == FolderId(2));
    let ops: Vec<ChangeOp> = fast.changes_since(0, 100).await.unwrap().changes.iter().map(|c| c.op).collect();
    assert_eq!(ops, vec![ChangeOp::Create, ChangeOp::Update, ChangeOp::Destroy]);

    // Order 2: expunge first, move second: the move must NOT resurrect or move the expunged row.
    let (_, acc, fast) = acct(&b, "mve2").await;
    let m = fast.append(INBOX, raw("m", None), Flags::default()).await.unwrap();
    let (slow, held) = slow_session(&b, acc).await;
    let (ex, mv) = staggered(held, async move { slow.expunge(m.id).await }, fast.move_message(m.id, FolderId(2))).await;
    ex.unwrap();
    assert!(matches!(mv, Err(Error::NotFound(_))), "{mv:?}");
    let d = fast.get_message(m.id).await.unwrap();
    assert!(d.deleted && d.folder == INBOX, "the expunged message stayed where it was");
    let ops: Vec<ChangeOp> = fast.changes_since(0, 100).await.unwrap().changes.iter().map(|c| c.op).collect();
    assert_eq!(ops, vec![ChangeOp::Create, ChangeOp::Destroy]);
}

/// `purge_deleted` holds the blob lock and has taken the refcount to zero (uncommitted) while a
/// second mailbox stores the same content: the store waits, then revives the row; the final
/// refcount is exactly 1 and `zero_since` is cleared (extends the PG2 in-flight GC test).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_deleted_versus_append_of_the_same_blob() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let (dom, acc, fast_x) = acct(&b, "pva").await;
    let y = open_on(&b, &Arc::new(PgRepo::new(store_of(&b))), new_account(&b, dom, "bob").await).await;
    let m = fast_x.append(INBOX, raw("shared-blob", None), Flags::default()).await.unwrap();
    fast_x.expunge_many_at(&[m.id], 100).await.unwrap();
    let (slow_x, held) = slow_session(&b, acc).await;
    let (purged, stored) = staggered(held, 
        async move { slow_x.purge_deleted(i64::MAX).await },
        y.append(INBOX, raw("shared-blob", None), Flags::default()),
    )
    .await;
    assert_eq!(purged.unwrap(), 1);
    stored.unwrap();
    assert_eq!(scalar(&b, "SELECT refcount FROM blobs").await, 1);
    assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE zero_since IS NOT NULL").await, 0);
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 1);
}

// ---------------------------------------------------------------- deadlock matrix and controls

fn rotated(docs: &[Bytes; 3], i: usize) -> Vec<Bytes> {
    let mut v = docs.to_vec();
    v.rotate_left(i % 3);
    if i % 2 == 1 {
        v.reverse();
    }
    v
}

/// 12 mailboxes x {delete_folder, purge_deleted, append_many, deliver_many} over the same three
/// blobs in rotated and reversed orders, all at once on a pool of 4. The repository has the
/// "hold after first blob lock" knob armed, which only has an effect on the (disabled) unordered
/// path: with the ascending rule in force it is inert. Expect no deadlock, no retry, exact
/// refcounts. (Mutation M-C / M-D: enable the unordered path.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_lock_order_matrix_has_no_deadlock_and_exact_refcounts() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let repo = repo_with(&b, Knobs { hold_after_first_blob_ms: 150, ..Knobs::default() });
    let docs = [raw("mx-a", None), raw("mx-b", None), raw("mx-c", None)];
    let (dom, _, _) = acct(&b, "mx0").await;
    let mut boxes = vec![];
    for i in 0..12 {
        let acc = new_account(&b, dom, &format!("u{i}")).await;
        let mb = open_on(&b, &repo, acc).await;
        // A folder with three expunged messages and an inbox with three expunged messages.
        let old = mb.create_folder(None, "Old").await.unwrap();
        let mut ids = vec![];
        for d in rotated(&docs, i) {
            ids.push(mb.append(old, d, Flags::default()).await.unwrap().id);
        }
        mb.expunge_many(&ids).await.unwrap();
        let mut ids = vec![];
        for d in rotated(&docs, i + 1) {
            ids.push(mb.append(INBOX, d, Flags::default()).await.unwrap().id);
        }
        mb.expunge_many_at(&ids, 100).await.unwrap();
        boxes.push((mb, old));
    }
    assert_eq!(scalar(&b, "SELECT max(refcount) FROM blobs").await, 24);
    let mut tasks = vec![];
    for (i, (mb, old)) in boxes.iter().enumerate() {
        let (m1, m2, m3, m4) = (mb.clone(), mb.clone(), mb.clone(), mb.clone());
        let old = *old;
        let (docs1, docs2) = (rotated(&docs, i + 2), rotated(&docs, i + 3));
        tasks.push(tokio::spawn(async move { m1.delete_folder(old).await.map(|_| ()) }));
        tasks.push(tokio::spawn(async move { m2.purge_deleted(i64::MAX).await.map(|_| ()) }));
        tasks.push(tokio::spawn(async move {
            m3.append_many(INBOX, docs1, Flags::default()).await.map(|_| ())
        }));
        tasks.push(tokio::spawn(async move {
            let items = docs2
                .into_iter()
                .enumerate()
                .map(|(k, raw)| DeliverItem {
                    raw,
                    flags: Flags::default(),
                    delivery_id: did((i * 3 + k) as u8 + 1),
                })
                .collect();
            m4.deliver_many(INBOX, items).await.map(|_| ())
        }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    let s = repo.stats();
    assert_eq!(s.deadlocks.load(Ordering::Relaxed), 0, "ordering must prevent every deadlock");
    assert_eq!(s.retries.load(Ordering::Relaxed), 0);
    assert_eq!(s.exhausted.load(Ordering::Relaxed), 0);
    // Live messages: 12 x (3 appended + 3 delivered); every blob is referenced by exactly them.
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages WHERE deleted_at IS NULL").await, 72);
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 72, "old folders and expunged inbox rows are gone");
    assert_eq!(
        scalar(
            &b,
            "SELECT count(*) FROM blobs bl WHERE bl.refcount <> \
             (SELECT count(*) FROM messages m WHERE m.blob_hash = bl.hash)"
        )
        .await,
        0,
        "refcount equals the number of referencing messages"
    );
    assert_eq!(scalars(&b, "SELECT refcount FROM blobs ORDER BY hash").await, vec![24, 24, 24]);
}

/// Two mailboxes with the same two blobs; the first repository locks them in the order the rows
/// come (`A`, `B`), the second in the reverse order, each pausing after its first lock: a REAL
/// deadlock. The retry loop turns the victim into a success. This is the control that proves each
/// path takes blob locks that can deadlock when the ordering rule is not applied.
#[derive(Clone, Copy, Debug)]
enum Path {
    DeleteFolder,
    PurgeDeleted,
    DeliverMany,
    AppendMany,
}

async fn forced_crossing(path: Path) {
    let b = pg_backend().await.unwrap();
    let hold = Knobs { unordered_blob_locks: true, hold_after_first_blob_ms: 300, ..Knobs::default() };
    let straight = repo_with(&b, hold);
    let reversed = repo_with(&b, Knobs { reverse_blob_lock_order: true, ..hold });
    // Inserts cross through the input order; deletes through the reverse knob.
    let (r1, r2) = match path {
        Path::DeleteFolder | Path::PurgeDeleted => (straight.clone(), reversed.clone()),
        Path::DeliverMany | Path::AppendMany => (straight.clone(), straight.clone()),
    };
    let (dom, a1, _) = acct(&b, "frc").await;
    let a2 = new_account(&b, dom, "bob").await;
    let (m1, m2) = (open_on(&b, &r1, a1).await, open_on(&b, &r2, a2).await);
    let (x, y) = (raw("fc-x", None), raw("fc-y", None));
    let (o1, o2) = match path {
        Path::DeleteFolder | Path::PurgeDeleted => (vec![x.clone(), y.clone()], vec![x.clone(), y.clone()]),
        Path::DeliverMany | Path::AppendMany => (vec![x.clone(), y.clone()], vec![y.clone(), x.clone()]),
    };
    // Setup through the default (ordered) repository: soft-deleted messages for the delete paths.
    let plain = repo_with(&b, Knobs::default());
    let mut folders = (INBOX, INBOX);
    if matches!(path, Path::DeleteFolder | Path::PurgeDeleted) {
        for (acc, order, slot) in [(a1, &o1, 0), (a2, &o2, 1)] {
            let mb = open_on(&b, &plain, acc).await;
            let f = if matches!(path, Path::DeleteFolder) {
                mb.create_folder(None, "Old").await.unwrap()
            } else {
                INBOX
            };
            let mut ids = vec![];
            for d in order {
                ids.push(mb.append(f, d.clone(), Flags::default()).await.unwrap().id);
            }
            mb.expunge_many_at(&ids, 100).await.unwrap();
            if slot == 0 { folders.0 = f } else { folders.1 = f }
        }
    }
    let run = |mb: Mailbox, order: Vec<Bytes>, f: FolderId, n: u8| async move {
        match path {
            Path::DeleteFolder => mb.delete_folder(f).await.map(|_| ()),
            Path::PurgeDeleted => mb.purge_deleted(i64::MAX).await.map(|_| ()),
            Path::AppendMany => mb.append_many(INBOX, order, Flags::default()).await.map(|_| ()),
            Path::DeliverMany => {
                let items = order
                    .into_iter()
                    .enumerate()
                    .map(|(k, raw)| DeliverItem { raw, flags: Flags::default(), delivery_id: did(n * 10 + k as u8) })
                    .collect();
                mb.deliver_many(INBOX, items).await.map(|_| ())
            }
        }
    };
    let started = Instant::now();
    let (t1, t2) = (
        tokio::spawn(run(m1.clone(), o1, folders.0, 1)),
        tokio::spawn(run(m2.clone(), o2, folders.1, 2)),
    );
    t1.await.unwrap().unwrap_or_else(|e| panic!("{path:?} session 1: {e}"));
    t2.await.unwrap().unwrap_or_else(|e| panic!("{path:?} session 2: {e}"));
    let deadlocks = r1.stats().deadlocks.load(Ordering::Relaxed) + r2.stats().deadlocks.load(Ordering::Relaxed);
    let retries = r1.stats().retries.load(Ordering::Relaxed) + r2.stats().retries.load(Ordering::Relaxed);
    let exhausted = r1.stats().exhausted.load(Ordering::Relaxed) + r2.stats().exhausted.load(Ordering::Relaxed);
    eprintln!("{path:?}: deadlocks={deadlocks} retries={retries} elapsed={:?}", started.elapsed());
    // r1 and r2 may be the same object for the insert paths: count once.
    let (deadlocks, retries, exhausted) = if Arc::ptr_eq(&r1, &r2) {
        (
            r1.stats().deadlocks.load(Ordering::Relaxed),
            r1.stats().retries.load(Ordering::Relaxed),
            r1.stats().exhausted.load(Ordering::Relaxed),
        )
    } else {
        (deadlocks, retries, exhausted)
    };
    assert!(deadlocks >= 1, "{path:?}: the crossed order must really deadlock (control)");
    assert!(retries >= 1, "{path:?}: and be retried");
    assert_eq!(exhausted, 0, "{path:?}");
    match path {
        Path::DeleteFolder | Path::PurgeDeleted => {
            assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 0, "{path:?}");
            assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE refcount = 0 AND zero_since IS NOT NULL").await, 2, "{path:?}");
        }
        Path::AppendMany | Path::DeliverMany => {
            assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 4, "{path:?}");
            assert_eq!(scalars(&b, "SELECT refcount FROM blobs ORDER BY hash").await, vec![2, 2], "{path:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forced_crossed_order_deadlocks_and_retries_delete_folder() {
    if pg_admin_url().is_none() {
        return;
    }
    forced_crossing(Path::DeleteFolder).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forced_crossed_order_deadlocks_and_retries_purge_deleted() {
    if pg_admin_url().is_none() {
        return;
    }
    forced_crossing(Path::PurgeDeleted).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forced_crossed_order_deadlocks_and_retries_deliver_many() {
    if pg_admin_url().is_none() {
        return;
    }
    forced_crossing(Path::DeliverMany).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forced_crossed_order_deadlocks_and_retries_append_many() {
    if pg_admin_url().is_none() {
        return;
    }
    forced_crossing(Path::AppendMany).await;
}

// ---------------------------------------------------------------- retry loop

/// Five consecutive 40P01 exhaust the loop: `Busy`, `exhausted == 1`, and the attempts left no
/// partial state (counters and rows untouched).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_consecutive_deadlocks_are_busy_and_leave_nothing_behind() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let repo = repo_with(&b, Knobs::default());
    let (_, acc, setup) = acct(&b, "exh").await;
    let mb = open_on(&b, &repo, acc).await;
    let f = setup.create_folder(None, "Del").await.unwrap();
    let kept = setup.append(INBOX, raw("kept", None), Flags::default()).await.unwrap();
    setup.expunge_many_at(&[kept.id], 100).await.unwrap();
    let state = || async {
        (
            scalar(&b, "SELECT next_msg_id FROM mailboxes").await,
            scalar(&b, "SELECT next_change_seq FROM mailboxes").await,
            scalar(&b, "SELECT next_folder_id FROM mailboxes").await,
            scalar(&b, "SELECT count(*) FROM messages").await,
            scalar(&b, "SELECT count(*) FROM folders").await,
            scalar(&b, "SELECT count(*) FROM blobs").await,
        )
    };
    let before = state().await;
    let busy = |r: Result<(), Error>| matches!(r, Err(Error::Busy(_)));
    macro_rules! exhaust {
        ($label:expr, $call:expr) => {{
            let exhausted_before = repo.stats().exhausted.load(Ordering::Relaxed);
            repo.inject_deadlocks_for_test(5);
            let r: Result<(), Error> = $call.await.map(|_| ());
            assert!(busy(r), "{}", $label);
            assert_eq!(repo.stats().exhausted.load(Ordering::Relaxed), exhausted_before + 1, "{}", $label);
            assert_eq!(state().await, before, "{}: no partial state", $label);
        }};
    }
    exhaust!("append_many", mb.append_many(INBOX, vec![raw("a", None), raw("b", None)], Flags::default()));
    exhaust!("deliver_many", mb.deliver_many(INBOX, vec![item("a", 1), item("b", 2)]));
    exhaust!("create_folder", mb.create_folder(None, "New"));
    exhaust!("delete_folder", mb.delete_folder(f));
    exhaust!("purge_deleted", mb.purge_deleted(i64::MAX));
    exhaust!("move_many", mb.move_many(&[kept.id], FolderId(2)));
    // The operation still works afterwards, with the counters where they were.
    let ok = mb.append(INBOX, raw("after", None), Flags::default()).await.unwrap();
    assert_eq!((ok.id.0, ok.uid), (2, 2));
}

/// Four injected deadlocks are retried and the fifth attempt succeeds, for the new paths too.
/// (Mutation M-E: `MAX_ATTEMPTS = 1`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn four_deadlocks_are_retried_and_the_operation_succeeds() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let repo = repo_with(&b, Knobs::default());
    let (_, acc, setup) = acct(&b, "retry").await;
    let mb = open_on(&b, &repo, acc).await;
    let f = setup.create_folder(None, "Del").await.unwrap();
    let doomed = setup.append(f, raw("doomed", None), Flags::default()).await.unwrap();
    setup.expunge(doomed.id).await.unwrap();
    let old = setup.append(INBOX, raw("old", None), Flags::default()).await.unwrap();
    setup.expunge_many_at(&[old.id], 100).await.unwrap();
    let mut expected_retries = 0;
    macro_rules! retried {
        ($label:expr, $call:expr) => {{
            repo.inject_deadlocks_for_test(4);
            let r = $call.await;
            assert!(r.is_ok(), "{}: {:?}", $label, r.err());
            expected_retries += 4;
            assert_eq!(repo.stats().retries.load(Ordering::Relaxed), expected_retries, "{}", $label);
            assert_eq!(repo.stats().exhausted.load(Ordering::Relaxed), 0, "{}", $label);
        }};
    }
    retried!("append_many", mb.append_many(INBOX, vec![raw("a", None), raw("b", None)], Flags::default()));
    retried!("deliver_many", mb.deliver_many(INBOX, vec![item("a", 1)]));
    retried!("delete_folder", mb.delete_folder(f));
    retried!("purge_deleted", mb.purge_deleted(i64::MAX));
    // Exactly one copy of each effect.
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages WHERE deleted_at IS NULL").await, 3);
    assert_eq!(scalar(&b, "SELECT count(*) FROM deliveries").await, 1);
    let seqs = scalars(&b, "SELECT seq FROM change_log ORDER BY seq").await;
    assert_eq!(seqs, (1..=seqs.len() as i64).collect::<Vec<_>>(), "retries leave no gap");
}

/// A reader polling during `deliver_many` writers on the real Postgres pool never sees a hole in
/// the change log, and the per-batch ids stay consecutive (see also the both-backend suite).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_deliver_many_batches_are_consecutive() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let (_, _, mb) = acct(&b, "cdm").await;
    let mut tasks = vec![];
    for t in 0..12u8 {
        let mb = mb.clone();
        tasks.push(tokio::spawn(async move {
            let items = (0..5u8).map(|k| item(&format!("b{t}-{k}"), t * 5 + k)).collect();
            mb.deliver_many(INBOX, items).await.unwrap()
        }));
    }
    let mut ids = vec![];
    for t in tasks {
        let out = t.await.unwrap();
        let refs: Vec<_> = out
            .iter()
            .map(|o| match o {
                DeliverOutcome::Created(r) => *r,
                other => panic!("{other:?}"),
            })
            .collect();
        for w in refs.windows(2) {
            assert_eq!((w[1].id.0, w[1].uid), (w[0].id.0 + 1, w[0].uid + 1));
        }
        ids.extend(refs.iter().map(|r| r.id.0));
    }
    ids.sort_unstable();
    assert_eq!(ids, (1..=60).collect::<Vec<_>>());
}

// ---------------------------------------------------------------- lock_blobs gives up

/// A GC that wins the race against the blob insert on every round exhausts the bounded loop of
/// `lock_blobs`: that is transient contention (`Busy`, retryable by the caller), not a storage
/// fault, and the transaction leaves nothing behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lock_blobs_giving_up_is_busy_and_leaves_nothing_behind() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let repo = repo_with(&b, Knobs { drop_blob_rows_after_insert: true, ..Knobs::default() });
    let (_, acc, setup) = acct(&b, "gu").await;
    let mb = open_on(&b, &repo, acc).await;
    let r = mb.append(INBOX, raw("a", None), Flags::default()).await;
    assert!(matches!(r, Err(Error::Busy(_))), "append: {r:?}");
    let r = mb.append_many(INBOX, vec![raw("b", None), raw("c", None)], Flags::default()).await;
    assert!(matches!(r, Err(Error::Busy(_))), "append_many: {r:?}");
    let r = mb.deliver_many(INBOX, vec![item("d", 1)]).await;
    assert!(matches!(r, Err(Error::Busy(_))), "deliver_many: {r:?}");
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 0);
    assert_eq!(scalar(&b, "SELECT count(*) FROM blobs").await, 0);
    assert_eq!(scalar(&b, "SELECT count(*) FROM deliveries").await, 0);
    assert!(setup.changes_since(0, 10).await.unwrap().changes.is_empty());
    // The same mailbox works through a repository without the knob.
    setup.append(INBOX, raw("a", None), Flags::default()).await.unwrap();
}
