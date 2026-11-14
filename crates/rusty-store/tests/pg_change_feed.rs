//! The change feed: one dedicated connection per process that LISTENs for the notifications of
//! migration 0017 and hands them to a sink. It must survive a lost connection (and say so with a
//! resync, because notifications sent while it was away are gone) and stop cleanly.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::pg::{pg_admin_url, PgTestDb};
use rusty_core::MailboxId;
use rusty_store::repo::pg::PgRepo;
use rusty_store::repo::{DirectoryRepo, MailboxRepo, NewAccount};
use rusty_store::{ChangeFeed, ChangeSink};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Mailbox(i64, i64),
    Jobs,
    Resync,
    Lost,
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Event>>);

impl Recorder {
    fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }
    fn count(&self, e: &Event) -> usize {
        self.events().iter().filter(|x| *x == e).count()
    }
    /// Wait until `pred` holds over the recorded events (at most 15 s).
    async fn until(&self, what: &str, pred: impl Fn(&[Event]) -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if pred(&self.events()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}; saw {:?}", self.events());
    }
}

impl ChangeSink for Recorder {
    fn mailbox_changed(&self, mailbox: MailboxId, seq: i64) {
        self.0.lock().unwrap().push(Event::Mailbox(mailbox.0, seq));
    }
    fn jobs_queued(&self) {
        self.0.lock().unwrap().push(Event::Jobs);
    }
    fn resync(&self) {
        self.0.lock().unwrap().push(Event::Resync);
    }
    fn lost(&self) {
        self.0.lock().unwrap().push(Event::Lost);
    }
}

async fn mailbox_of(repo: &PgRepo, tag: &str) -> MailboxId {
    let dom = repo.create_domain(&format!("{tag}.test")).await.unwrap();
    let acc = repo
        .create_account(&NewAccount { local: "alice".into(), domain: dom, plan: None, password_hash: None })
        .await
        .unwrap();
    repo.mailbox_for_account(acc).await.unwrap()
}

async fn listener_backends(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() \
         AND pid <> pg_backend_pid() AND query LIKE 'LISTEN%'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn announcements_reach_the_sink_after_the_feed_subscribed() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;
    let repo = PgRepo::new(store.clone());
    let mb = mailbox_of(&repo, "feed").await;

    let rec = Arc::new(Recorder::default());
    let stop = CancellationToken::new();
    let feed = store.change_feed();
    let task = tokio::spawn({
        let (rec, stop) = (rec.clone(), stop.clone());
        async move { feed.run(rec, stop).await }
    });
    // `resync` is announced once the LISTEN is in place: from then on nothing can be missed.
    rec.until("the first resync", |e| e.contains(&Event::Resync)).await;

    repo.create_folder(mb, None, "Projects").await.unwrap();
    let seq: i64 = sqlx::query_scalar("SELECT max(seq) FROM change_log WHERE mailbox_id = $1")
        .bind(mb.0)
        .fetch_one(store.pool())
        .await
        .unwrap();
    rec.until("the mailbox announcement", |e| e.contains(&Event::Mailbox(mb.0, seq))).await;

    sqlx::query("INSERT INTO jobs (kind, payload, run_after, created_at, updated_at) VALUES ('t.feed', '{}', 0, 0, 0)")
        .execute(store.pool())
        .await
        .unwrap();
    rec.until("the job announcement", |e| e.contains(&Event::Jobs)).await;

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("the feed must stop").unwrap().unwrap();
    db.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_connection_is_reported_and_the_feed_comes_back_with_a_resync() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;
    let repo = PgRepo::new(store.clone());
    let mb = mailbox_of(&repo, "reconnect").await;

    let rec = Arc::new(Recorder::default());
    let stop = CancellationToken::new();
    let feed = store.change_feed();
    let task = tokio::spawn({
        let (rec, stop) = (rec.clone(), stop.clone());
        async move { feed.run(rec, stop).await }
    });
    rec.until("the first resync", |e| rec_has(e, &Event::Resync)).await;
    assert_eq!(listener_backends(store.pool()).await, 1, "exactly one LISTEN connection");

    // The server kills the listener's connection (restart, failover, idle kill).
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() \
         AND pid <> pg_backend_pid() AND query LIKE 'LISTEN%'",
    )
    .execute(store.pool())
    .await
    .unwrap();

    rec.until("the loss", |e| rec_has(e, &Event::Lost)).await;
    rec.until("the second resync", |e| e.iter().filter(|x| **x == Event::Resync).count() >= 2).await;

    // After the reconnect, announcements flow again.
    repo.create_folder(mb, None, "After").await.unwrap();
    rec.until("an announcement after the reconnect", |e| {
        let last_resync = e.iter().rposition(|x| *x == Event::Resync).unwrap();
        e[last_resync..].iter().any(|x| matches!(x, Event::Mailbox(m, _) if *m == mb.0))
    })
    .await;
    assert_eq!(rec.count(&Event::Lost), 1);

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("the feed must stop").unwrap().unwrap();
    db.cleanup().await;
}

fn rec_has(events: &[Event], e: &Event) -> bool {
    events.contains(e)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_the_feed_closes_its_connection() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;

    let rec = Arc::new(Recorder::default());
    let stop = CancellationToken::new();
    let feed = store.change_feed();
    let task = tokio::spawn({
        let (rec, stop) = (rec.clone(), stop.clone());
        async move { feed.run(rec, stop).await }
    });
    rec.until("the first resync", |e| rec_has(e, &Event::Resync)).await;
    assert_eq!(listener_backends(store.pool()).await, 1);

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("the feed must stop").unwrap().unwrap();

    let end = Instant::now() + Duration::from_secs(5);
    while listener_backends(store.pool()).await != 0 {
        assert!(Instant::now() < end, "the LISTEN connection is still open after the feed stopped");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    db.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_feed_that_cannot_connect_keeps_trying_and_still_stops() {
    let Some(admin) = pg_admin_url() else { return };
    // A runtime role (not a superuser, so a connection limit applies to it) with a store that is already up.
    let db = PgTestDb::create_with_roles(&admin).await;
    let store = rusty_store::PgStore::connect(&db.role_url("rusty_app")).await.unwrap();
    // Every NEW connection of this one database is refused from now on (the store keeps the ones it has):
    // the feed must report the loss, not panic, and still stop on request.
    let admin_pool = sqlx::PgPool::connect(db.admin_db_url()).await.unwrap();
    // `name` is generated by the test helper from a fixed prefix and lowercase hex.
    sqlx::query(sqlx::AssertSqlSafe(format!("ALTER DATABASE \"{}\" CONNECTION LIMIT 0", db.name())))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;

    let rec = Arc::new(Recorder::default());
    let stop = CancellationToken::new();
    let feed = store.change_feed();
    let task = tokio::spawn({
        let (rec, stop) = (rec.clone(), stop.clone());
        async move { feed.run(rec, stop).await }
    });
    rec.until("a reported loss", |e| rec_has(e, &Event::Lost)).await;
    assert_eq!(rec.count(&Event::Resync), 0, "no resync without a subscription");

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("the feed must stop").unwrap().unwrap();
    db.cleanup().await;
}
