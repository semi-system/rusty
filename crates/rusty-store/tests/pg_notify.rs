//! Cross-process notifications: the database itself announces, at commit, that a mailbox changed
//! (`rusty_mailbox`, payload `<mailbox id>:<newest seq>`) and that a job was queued (`rusty_jobs`).
//! Another process (IMAP IDLE, the outbound worker) listens; nothing here depends on one process.
//! `NOTIFY` is delivered at commit, so a rolled-back change is never announced.

mod common;

use std::time::Duration;

use common::pg::{pg_admin_url, PgTestDb};
use rusty_core::MailboxId;
use rusty_store::repo::pg::PgRepo;
use rusty_store::repo::{DirectoryRepo, MailboxRepo, NewAccount};
use sqlx::postgres::PgListener;

const SHORT: Duration = Duration::from_millis(700);
const LONG: Duration = Duration::from_secs(10);

async fn mailbox_of(repo: &PgRepo, tag: &str) -> MailboxId {
    let dom = repo.create_domain(&format!("{tag}.test")).await.unwrap();
    let acc = repo
        .create_account(&NewAccount { local: "alice".into(), domain: dom, plan: None, password_hash: None })
        .await
        .unwrap();
    repo.mailbox_for_account(acc).await.unwrap()
}

async fn listener(url: &str) -> PgListener {
    let mut l = PgListener::connect(url).await.unwrap();
    l.listen_all(["rusty_mailbox", "rusty_jobs"]).await.unwrap();
    l
}

/// The next notification within `wait`, or `None`.
async fn next(l: &mut PgListener, wait: Duration) -> Option<(String, String)> {
    match tokio::time::timeout(wait, l.recv()).await {
        Ok(n) => {
            let n = n.unwrap();
            Some((n.channel().to_owned(), n.payload().to_owned()))
        }
        Err(_) => None,
    }
}

async fn newest_seq(store: &rusty_store::PgStore, mb: MailboxId) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(max(seq), 0) FROM change_log WHERE mailbox_id = $1")
        .bind(mb.0)
        .fetch_one(store.pool())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_change_is_announced_with_the_mailbox_and_its_newest_seq() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;
    let repo = PgRepo::new(store.clone());
    let mb = mailbox_of(&repo, "commit").await;
    // Drain what the set-up announced (account and mailbox creation write the change log too).
    let mut l = listener(db.url()).await;
    while next(&mut l, SHORT).await.is_some() {}

    repo.create_folder(mb, None, "Projects").await.unwrap();

    let seq = newest_seq(&store, mb).await;
    let got = next(&mut l, LONG).await.expect("a committed change must be announced");
    assert_eq!(got, ("rusty_mailbox".to_owned(), format!("{}:{seq}", mb.0)));
    db.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rolled_back_change_is_never_announced() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;
    let repo = PgRepo::new(store.clone());
    let mb = mailbox_of(&repo, "rollback").await;
    let mut l = listener(db.url()).await;
    while next(&mut l, SHORT).await.is_some() {}

    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("INSERT INTO change_log (mailbox_id, seq, kind, item_id, op) VALUES ($1, 1000000, 'message', 1, 'create')")
        .bind(mb.0)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(next(&mut l, SHORT).await, None, "a rolled-back change was announced");

    // Positive control: the same listener does hear a committed change.
    repo.create_folder(mb, None, "Control").await.unwrap();
    assert!(next(&mut l, LONG).await.is_some(), "the listener is deaf, the test proves nothing");
    db.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_statement_adding_many_changes_sends_one_notification_per_mailbox() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;
    let repo = PgRepo::new(store.clone());
    let mb = mailbox_of(&repo, "batch").await;
    let base = newest_seq(&store, mb).await;
    let mut l = listener(db.url()).await;
    while next(&mut l, SHORT).await.is_some() {}

    sqlx::query(
        "INSERT INTO change_log (mailbox_id, seq, kind, item_id, op) \
         SELECT $1, g, 'message', g, 'create' FROM generate_series($2::bigint, $3::bigint) g",
    )
    .bind(mb.0)
    .bind(base + 1)
    .bind(base + 200)
    .execute(store.pool())
    .await
    .unwrap();

    let got = next(&mut l, LONG).await.expect("the batch must be announced");
    assert_eq!(got, ("rusty_mailbox".to_owned(), format!("{}:{}", mb.0, base + 200)), "newest seq of the batch");
    assert_eq!(next(&mut l, SHORT).await, None, "200 rows must not be 200 notifications");
    db.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_job_is_announced() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;
    let mut l = listener(db.url()).await;
    while next(&mut l, SHORT).await.is_some() {}

    sqlx::query(
        "INSERT INTO jobs (kind, payload, run_after, created_at, updated_at) VALUES ('t.notify', '{}', 0, 0, 0)",
    )
    .execute(store.pool())
    .await
    .unwrap();

    let got = next(&mut l, LONG).await.expect("a queued job must be announced");
    assert_eq!(got.0, "rusty_jobs");
    db.cleanup().await;
}
