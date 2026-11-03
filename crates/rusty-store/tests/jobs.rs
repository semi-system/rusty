mod common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use common::{file_store, mem};
use rusty_core::{DomainId, Error, JobId};
use rusty_store::{JobQueue, SqliteStore, SqliteTx};
use serde_json::{json, Value};

const STALE: &str = "stale job epoch";

async fn q() -> (SqliteStore, JobQueue) {
    let s = mem().await;
    let jq = JobQueue::new(s.clone());
    (s, jq)
}

async fn state(s: &SqliteStore, id: JobId) -> String {
    sqlx::query_scalar("SELECT state FROM jobs WHERE id = ?")
        .bind(id.0)
        .fetch_one(s.pool())
        .await
        .unwrap()
}

async fn num(s: &SqliteStore, sql: &str, id: JobId) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id.0)
        .fetch_one(s.pool())
        .await
        .unwrap()
}

fn is_stale(r: &rusty_core::Result<()>) -> bool {
    matches!(r, Err(Error::Conflict(m)) if m == STALE)
}

async fn enq(jq: &JobQueue, kind: &str, run_after: Option<i64>, now: i64) -> JobId {
    jq.enqueue_at(kind, None, &json!({"k": kind}), run_after, now)
        .await
        .unwrap()
}

#[tokio::test]
async fn enqueue_validates_kind_and_payload() {
    let (s, jq) = q().await;
    let long = "a".repeat(65);
    let ok64 = "a".repeat(64);
    for bad in [
        "",
        "Upper",
        "has space",
        "semi;colon",
        "ünï",
        "tab\t",
        &long,
    ] {
        let r = jq.enqueue(bad, None, &json!({}), None).await;
        match r {
            Err(Error::Invalid(m)) => assert!(!m.contains(bad) || bad.is_empty(), "echoed kind"),
            other => panic!("kind {bad:?}: expected Invalid, got {other:?}"),
        }
    }
    jq.enqueue(&ok64, None, &json!({}), None).await.unwrap();
    jq.enqueue("mail.index-1_x", None, &json!({}), None)
        .await
        .unwrap();

    // Payload: exactly 64 KiB serialized is accepted, one more byte is not.
    let at_limit = Value::String("a".repeat(65536 - 2));
    jq.enqueue("p", None, &at_limit, None).await.unwrap();
    let over = Value::String("a".repeat(65536 - 1));
    assert!(matches!(
        jq.enqueue("p", None, &over, None).await,
        Err(Error::Invalid(_))
    ));
    // Multi-byte text counts bytes, not characters.
    let wide = Value::String("é".repeat(40000));
    assert!(matches!(
        jq.enqueue("p", None, &wide, None).await,
        Err(Error::Invalid(_))
    ));
    // A payload nested deeper than serde_json can parse back would poison claim: refused.
    let mut deep = json!(1);
    for _ in 0..200 {
        deep = Value::Array(vec![deep]);
    }
    assert!(matches!(
        jq.enqueue("p", None, &deep, None).await,
        Err(Error::Invalid(_))
    ));
    // Attempt cap bounds.
    for n in [0u32, 21] {
        assert!(matches!(
            jq.enqueue_with_max_attempts("p", None, &json!({}), None, n)
                .await,
            Err(Error::Invalid(_))
        ));
    }
    jq.enqueue_with_max_attempts("p", None, &json!({}), None, 20)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(s.pool())
        .await
        .unwrap();
    assert_eq!(n, 4);
}

#[tokio::test]
async fn claim_picks_oldest_eligible_and_respects_run_after() {
    let (_s, jq) = q().await;
    let later = enq(&jq, "t", Some(1100), 1000).await;
    let b = enq(&jq, "t", Some(600), 1000).await;
    let a = enq(&jq, "t", Some(500), 1000).await;
    let c = enq(&jq, "t", None, 1000).await; // run_after = 1000
    let w = |n| jq.claim_at("w1", &["t"], 86400, n);
    assert_eq!(w(1000).await.unwrap().unwrap().id, a);
    assert_eq!(w(1000).await.unwrap().unwrap().id, b);
    let third = w(1000).await.unwrap().unwrap();
    assert_eq!(third.id, c);
    assert_eq!((third.epoch, third.attempts), (1, 1));
    assert!(w(1000).await.unwrap().is_none(), "run_after in the future");
    assert!(w(1099).await.unwrap().is_none());
    assert_eq!(w(1100).await.unwrap().unwrap().id, later);
    assert!(w(1100).await.unwrap().is_none());
}

#[tokio::test]
async fn claim_filters_by_kind() {
    let (_s, jq) = q().await;
    let a = enq(&jq, "alpha", None, 10).await;
    let b = enq(&jq, "beta", None, 10).await;
    let c = enq(&jq, "gamma", None, 10).await;
    let got = jq.claim_at("w", &["beta"], 30, 10).await.unwrap().unwrap();
    assert_eq!((got.id, got.kind.as_str()), (b, "beta"));
    assert!(jq.claim_at("w", &["beta"], 30, 10).await.unwrap().is_none());
    let got = jq
        .claim_at("w", &["gamma", "alpha"], 30, 10)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.id, a);
    assert_eq!(
        jq.claim_at("w", &["gamma"], 30, 10)
            .await
            .unwrap()
            .unwrap()
            .id,
        c
    );
    // Hostile or empty filters and lease/worker values are refused.
    assert!(matches!(
        jq.claim_at("w", &[], 30, 10).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        jq.claim_at("w", &["x' OR 1=1 --"], 30, 10).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        jq.claim_at("", &["t"], 30, 10).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        jq.claim_at("w\n", &["t"], 30, 10).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        jq.claim_at("w", &["t"], 0, 10).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        jq.claim_at("w", &["t"], 86401, 10).await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_never_claim_the_same_job() {
    let dir = tempfile::tempdir().unwrap();
    let s = file_store(dir.path()).await;
    let jq = JobQueue::new(s.clone());
    let mut all = HashSet::new();
    for i in 0..200 {
        all.insert(
            jq.enqueue("work", None, &json!({ "i": i }), None)
                .await
                .unwrap()
                .0,
        );
    }
    let claimed = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = Vec::new();
    for w in 0..8 {
        let jq = JobQueue::new(s.clone());
        let claimed = claimed.clone();
        tasks.push(tokio::spawn(async move {
            let name = format!("worker-{w}");
            while let Some(job) = jq.claim(&name, &["work"], 60).await.unwrap() {
                claimed.lock().unwrap().push(job.id.0);
                jq.complete(job.id, job.epoch).await.unwrap();
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let got = claimed.lock().unwrap().clone();
    assert_eq!(got.len(), 200, "every job claimed");
    let set: HashSet<i64> = got.into_iter().collect();
    assert_eq!(set, all, "each job claimed exactly once");
    let done: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE state = 'done'")
        .fetch_one(s.pool())
        .await
        .unwrap();
    assert_eq!(done, 200);
}

#[tokio::test]
async fn expired_lease_job_is_reclaimed_with_higher_epoch() {
    let (s, jq) = q().await;
    let id = enq(&jq, "t", None, 1000).await;
    let first = jq.claim_at("w1", &["t"], 30, 1000).await.unwrap().unwrap();
    assert_eq!((first.epoch, first.attempts), (1, 1));
    assert!(jq.claim_at("w2", &["t"], 30, 1029).await.unwrap().is_none());
    let second = jq.claim_at("w2", &["t"], 30, 1030).await.unwrap().unwrap();
    assert_eq!(second.id, id);
    assert_eq!((second.epoch, second.attempts), (2, 2));
    assert_eq!(state(&s, id).await, "running");
    let owner: String = sqlx::query_scalar("SELECT lease_owner FROM jobs WHERE id = ?")
        .bind(id.0)
        .fetch_one(s.pool())
        .await
        .unwrap();
    assert_eq!(owner, "w2");
}

#[tokio::test]
async fn zombie_worker_cannot_heartbeat_complete_or_fail() {
    let (s, jq) = q().await;
    let id = enq(&jq, "t", None, 1000).await;
    let zombie = jq.claim_at("w1", &["t"], 30, 1000).await.unwrap().unwrap();
    let owner = jq.claim_at("w2", &["t"], 30, 1040).await.unwrap().unwrap();
    assert!(owner.epoch > zombie.epoch);
    assert!(is_stale(&jq.heartbeat_at(id, zombie.epoch, 30, 1041).await));
    assert!(is_stale(&jq.complete_at(id, zombie.epoch, 1041).await));
    assert!(is_stale(
        &jq.fail_at(id, zombie.epoch, "boom", 10, 1041).await
    ));
    // Nothing changed for the real owner.
    assert_eq!(state(&s, id).await, "running");
    assert_eq!(num(&s, "SELECT epoch FROM jobs WHERE id = ?", id).await, 2);
    jq.heartbeat_at(id, owner.epoch, 30, 1041).await.unwrap();
    jq.complete_at(id, owner.epoch, 1042).await.unwrap();
    assert_eq!(state(&s, id).await, "done");
    // After completion even the former owner is stale, as is an unknown id.
    assert!(is_stale(&jq.complete_at(id, owner.epoch, 1043).await));
    assert!(is_stale(&jq.heartbeat_at(JobId(9999), 1, 30, 1043).await));
    // An owner whose lease lapsed (nobody re-claimed yet) is also refused: the lease decides.
    let id2 = enq(&jq, "t", None, 2000).await;
    let j = jq.claim_at("w1", &["t"], 30, 2000).await.unwrap().unwrap();
    assert!(is_stale(&jq.heartbeat_at(id2, j.epoch, 30, 2030).await));
    assert!(is_stale(&jq.complete_at(id2, j.epoch, 2030).await));
}

#[tokio::test]
async fn fence_rejects_stale_epoch_inside_transaction() {
    let (s, jq) = q().await;
    let id = enq(&jq, "t", None, 1000).await;
    let zombie = jq.claim_at("w1", &["t"], 30, 1000).await.unwrap().unwrap();

    // The current owner passes and its side effect commits atomically with the fence check.
    let mut tx = SqliteTx::begin(&s).await.unwrap();
    JobQueue::fence_at(&mut tx, id, zombie.epoch, 1010)
        .await
        .unwrap();
    sqlx::query("INSERT INTO domains (name, created_at) VALUES ('fenced-ok.test', 0)")
        .execute(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM domains WHERE name = 'fenced-ok.test'")
        .fetch_one(s.pool())
        .await
        .unwrap();
    assert_eq!(n, 1);

    // An expired lease fails the fence even before anyone re-claims.
    let mut tx = SqliteTx::begin(&s).await.unwrap();
    let r = JobQueue::fence_at(&mut tx, id, zombie.epoch, 1030).await;
    assert!(is_stale(&r));
    tx.rollback().await.unwrap();

    // After a re-claim the old epoch is rejected, the side effect is rolled back with it.
    let owner = jq.claim_at("w2", &["t"], 30, 1030).await.unwrap().unwrap();
    let mut tx = SqliteTx::begin(&s).await.unwrap();
    sqlx::query("INSERT INTO domains (name, created_at) VALUES ('fenced-bad.test', 0)")
        .execute(tx.conn())
        .await
        .unwrap();
    let r = JobQueue::fence_at(&mut tx, id, zombie.epoch, 1031).await;
    assert!(is_stale(&r));
    drop(tx); // the caller abandons the transaction on a failed fence
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM domains WHERE name = 'fenced-bad.test'")
        .fetch_one(s.pool())
        .await
        .unwrap();
    assert_eq!(n, 0, "side effect rolled back");

    // The new owner passes; unknown ids and finished jobs do not.
    let mut tx = SqliteTx::begin(&s).await.unwrap();
    JobQueue::fence_at(&mut tx, id, owner.epoch, 1031)
        .await
        .unwrap();
    assert!(is_stale(
        &JobQueue::fence_at(&mut tx, JobId(4242), 1, 1031).await
    ));
    tx.rollback().await.unwrap();
    jq.complete_at(id, owner.epoch, 1032).await.unwrap();
    let mut tx = SqliteTx::begin(&s).await.unwrap();
    assert!(is_stale(
        &JobQueue::fence_at(&mut tx, id, owner.epoch, 1033).await
    ));
    tx.rollback().await.unwrap();

    // The DB-clock variant works against a freshly claimed job.
    let id3 = jq.enqueue("t", None, &json!({}), None).await.unwrap();
    let j = jq.claim("w3", &["t"], 60).await.unwrap().unwrap();
    assert_eq!(j.id, id3);
    let mut tx = SqliteTx::begin(&s).await.unwrap();
    JobQueue::fence(&mut tx, id3, j.epoch).await.unwrap();
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn fail_requeues_with_backoff_then_dead_at_max_attempts() {
    let (s, jq) = q().await;
    let id = jq
        .enqueue_with_max_attempts_at("t", None, &json!({}), None, 2, 1000)
        .await
        .unwrap();
    let j1 = jq.claim_at("w", &["t"], 30, 1000).await.unwrap().unwrap();
    jq.fail_at(id, j1.epoch, "first", 60, 1001).await.unwrap();
    assert_eq!(state(&s, id).await, "queued");
    assert_eq!(
        num(&s, "SELECT run_after FROM jobs WHERE id = ?", id).await,
        1061
    );
    assert_eq!(
        num(&s, "SELECT lease_owner IS NULL FROM jobs WHERE id = ?", id).await,
        1
    );
    assert!(jq.claim_at("w", &["t"], 30, 1060).await.unwrap().is_none());
    let j2 = jq.claim_at("w", &["t"], 30, 1061).await.unwrap().unwrap();
    assert_eq!((j2.epoch, j2.attempts), (2, 2));
    jq.fail_at(id, j2.epoch, "second", 60, 1062).await.unwrap();
    assert_eq!(state(&s, id).await, "dead");
    let err: String = sqlx::query_scalar("SELECT last_error FROM jobs WHERE id = ?")
        .bind(id.0)
        .fetch_one(s.pool())
        .await
        .unwrap();
    assert_eq!(err, "second");
    assert!(jq
        .claim_at("w", &["t"], 30, 1_000_000)
        .await
        .unwrap()
        .is_none());

    // A job whose lease lapses on its last attempt goes dead instead of looping forever.
    let id2 = jq
        .enqueue_with_max_attempts_at("t", None, &json!({}), None, 1, 2000)
        .await
        .unwrap();
    jq.claim_at("w", &["t"], 30, 2000).await.unwrap().unwrap();
    assert!(jq.claim_at("w", &["t"], 30, 2100).await.unwrap().is_none());
    assert_eq!(state(&s, id2).await, "dead");
    assert_eq!(
        num(
            &s,
            "SELECT lease_expires_at IS NULL FROM jobs WHERE id = ?",
            id2
        )
        .await,
        1
    );
}

#[tokio::test]
async fn poisoned_stored_payload_cannot_block_claim() {
    let (s, jq) = q().await;
    // Valid JSON for SQLite (depth limit 1000) but beyond serde_json's recursion limit.
    let deep = format!("{}1{}", "[".repeat(300), "]".repeat(300));
    let bad: i64 = sqlx::query_scalar(
        "INSERT INTO jobs (kind, state, payload, attempts, max_attempts, run_after, created_at, updated_at) \
         VALUES ('t', 'queued', ?, 0, 5, 0, 0, 0) RETURNING id",
    )
    .bind(&deep)
    .fetch_one(s.pool())
    .await
    .unwrap();
    let good = enq(&jq, "t", Some(5), 1000).await;
    let got = jq.claim_at("w", &["t"], 30, 1000).await.unwrap().unwrap();
    assert_eq!(got.id, good);
    assert_eq!(state(&s, JobId(bad)).await, "dead");
}

#[tokio::test]
async fn last_error_is_truncated_and_sanitized() {
    let (s, jq) = q().await;
    let id = jq
        .enqueue_with_max_attempts_at("t", None, &json!({}), None, 5, 1000)
        .await
        .unwrap();
    let j = jq.claim_at("w", &["t"], 30, 1000).await.unwrap().unwrap();
    let nasty = format!(
        "bad\u{0}\u{1b}[31m line1\r\nline2\u{202e}\u{2028}\u{7f}\t{}{}",
        "é".repeat(400),
        "x".repeat(1000)
    );
    jq.fail_at(id, j.epoch, &nasty, 1, 1001).await.unwrap();
    let e: String = sqlx::query_scalar("SELECT last_error FROM jobs WHERE id = ?")
        .bind(id.0)
        .fetch_one(s.pool())
        .await
        .unwrap();
    assert!(e.len() <= 512, "{}", e.len());
    assert!(e.starts_with("bad[31m line1line2"), "{e:?}");
    assert!(
        !e.chars()
            .any(|c| c.is_control() || matches!(c, '\u{202e}' | '\u{2028}')),
        "{e:?}"
    );
    // The worker's text is data: it never appears in an Error message.
    let r = jq.fail_at(id, 99, "SECRET-TEXT", 1, 1002).await;
    match r {
        Err(Error::Conflict(m)) => assert!(!m.contains("SECRET")),
        other => panic!("{other:?}"),
    }
    // Empty and all-control messages are stored as an empty string, still valid.
    let j = jq.claim_at("w", &["t"], 30, 1100).await.unwrap().unwrap();
    jq.fail_at(id, j.epoch, "\u{0}\u{1}", 1, 1101)
        .await
        .unwrap();
    let e: String = sqlx::query_scalar("SELECT last_error FROM jobs WHERE id = ?")
        .bind(id.0)
        .fetch_one(s.pool())
        .await
        .unwrap();
    assert_eq!(e, "");
}

#[tokio::test]
async fn heartbeat_extends_lease() {
    let (s, jq) = q().await;
    let id = enq(&jq, "t", None, 1000).await;
    let j = jq.claim_at("w", &["t"], 30, 1000).await.unwrap().unwrap();
    jq.heartbeat_at(id, j.epoch, 100, 1020).await.unwrap();
    assert_eq!(
        num(&s, "SELECT lease_expires_at FROM jobs WHERE id = ?", id).await,
        1120
    );
    // Not reclaimable at the original expiry, reclaimable at the extended one.
    assert!(jq.claim_at("w2", &["t"], 30, 1030).await.unwrap().is_none());
    assert!(jq.claim_at("w2", &["t"], 30, 1119).await.unwrap().is_none());
    let again = jq.claim_at("w2", &["t"], 30, 1120).await.unwrap().unwrap();
    assert_eq!(again.epoch, 2);
    // The epoch is unchanged by heartbeat, and bad lease lengths are refused.
    assert!(matches!(
        jq.heartbeat_at(id, again.epoch, 0, 1121).await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test]
async fn purge_finished_only_removes_old_done_dead() {
    let (s, jq) = q().await;
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(enq(&jq, "t", None, 100).await);
    }
    let [done_old, dead_old, done_new, queued_old, running_old] = ids[..] else {
        panic!()
    };
    for (id, st, at) in [
        (done_old, "done", 100),
        (dead_old, "dead", 100),
        (done_new, "done", 5000),
    ] {
        sqlx::query("UPDATE jobs SET state = ?, updated_at = ? WHERE id = ?")
            .bind(st)
            .bind(at)
            .bind(id.0)
            .execute(s.pool())
            .await
            .unwrap();
    }
    sqlx::query(
        "UPDATE jobs SET state = 'running', lease_owner = 'w', lease_expires_at = 99999, \
         updated_at = 100 WHERE id = ?",
    )
    .bind(running_old.0)
    .execute(s.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET updated_at = 100 WHERE id = ?")
        .bind(queued_old.0)
        .execute(s.pool())
        .await
        .unwrap();
    // Strict cutoff: updated_at < older_than.
    assert_eq!(jq.purge_finished(100).await.unwrap(), 0);
    assert_eq!(jq.purge_finished(1000).await.unwrap(), 2);
    let left: Vec<i64> = sqlx::query_scalar("SELECT id FROM jobs ORDER BY id")
        .fetch_all(s.pool())
        .await
        .unwrap();
    assert_eq!(left, vec![done_new.0, queued_old.0, running_old.0]);
}

#[tokio::test]
async fn purge_finished_batches_beyond_1000() {
    let (s, jq) = q().await;
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2300) \
         INSERT INTO jobs (kind, state, payload, attempts, max_attempts, run_after, \
                           created_at, updated_at) \
         SELECT 't', 'done', '{}', 1, 5, 0, 0, 1 FROM n",
    )
    .execute(s.pool())
    .await
    .unwrap();
    assert_eq!(jq.purge_finished(100).await.unwrap(), 2300);
}

#[tokio::test]
async fn domain_fk_enforced() {
    let (s, jq) = q().await;
    let r = jq
        .enqueue_at("t", Some(DomainId(77)), &json!({}), None, 10)
        .await;
    assert!(matches!(r, Err(Error::Conflict(_))), "{r:?}");
    sqlx::query("INSERT INTO domains (id, name, created_at) VALUES (7, 'd.test', 0)")
        .execute(s.pool())
        .await
        .unwrap();
    let id = jq
        .enqueue_at("t", Some(DomainId(7)), &json!({"a": 1}), None, 10)
        .await
        .unwrap();
    let j = jq.claim_at("w", &["t"], 30, 10).await.unwrap().unwrap();
    assert_eq!(j.id, id);
    assert_eq!(j.domain, Some(DomainId(7)));
    assert_eq!(j.payload, json!({"a": 1}));
    // A referenced domain cannot be deleted from under its jobs.
    assert!(sqlx::query("DELETE FROM domains WHERE id = 7")
        .execute(s.pool())
        .await
        .is_err());
}

#[tokio::test]
async fn schema_rejects_inconsistent_rows() {
    let (s, _jq) = q().await;
    let ins = |state: &str, lease: &str, extra: &str| {
        format!(
            "INSERT INTO jobs (kind, state, payload, attempts, max_attempts, run_after, \
             lease_owner, lease_expires_at, created_at, updated_at{}) \
             VALUES ('t', '{state}', '{{}}', 0, 5, 0, {lease}, 0, 0{})",
            extra.split('|').next().unwrap_or(""),
            extra.split('|').nth(1).unwrap_or("")
        )
    };
    let pool = s.pool().clone();
    let ok = |sql: String| {
        let pool = pool.clone();
        async move { sqlx::query(&sql).execute(&pool).await.is_ok() }
    };
    // running needs both lease columns; others need neither.
    assert!(ok(ins("running", "'w', 100", "")).await);
    assert!(!ok(ins("running", "NULL, NULL", "")).await);
    assert!(!ok(ins("running", "'w', NULL", "")).await);
    assert!(!ok(ins("queued", "'w', 100", "")).await);
    assert!(!ok(ins("done", "'w', 100", "")).await);
    assert!(!ok(ins("dead", "'w', 100", "")).await);
    assert!(ok(ins("done", "NULL, NULL", "")).await);
    assert!(ok(ins("failed", "NULL, NULL", "")).await);
    assert!(!ok(ins("bogus", "NULL, NULL", "")).await);
    // last_error length, attempts bounds, kind and payload constraints.
    let long = "e".repeat(513);
    assert!(
        !ok(ins(
            "queued",
            "NULL, NULL",
            &format!(", last_error|, '{long}'")
        ))
        .await
    );
    assert!(
        ok(ins(
            "queued",
            "NULL, NULL",
            &format!(", last_error|, '{}'", "e".repeat(512))
        ))
        .await
    );
    let raw = |kind: &str, payload: &str, attempts: i64, max: i64| {
        format!(
            "INSERT INTO jobs (kind, state, payload, attempts, max_attempts, run_after, \
             created_at, updated_at) VALUES ('{kind}', 'queued', '{payload}', {attempts}, {max}, 0, 0, 0)"
        )
    };
    assert!(ok(raw("t", "{}", 0, 5)).await);
    assert!(!ok(raw("Bad Kind", "{}", 0, 5)).await);
    assert!(!ok(raw("", "{}", 0, 5)).await);
    assert!(!ok(raw(&"k".repeat(65), "{}", 0, 5)).await);
    assert!(!ok(raw("t", "{not json", 0, 5)).await);
    assert!(!ok(raw("t", &format!("\"{}\"", "a".repeat(65535)), 0, 5)).await);
    assert!(ok(raw("t", &format!("\"{}\"", "a".repeat(65534)), 0, 5)).await);
    assert!(!ok(raw("t", "{}", 6, 5)).await);
    assert!(!ok(raw("t", "{}", -1, 5)).await);
    assert!(!ok(raw("t", "{}", 0, 0)).await);
    assert!(!ok(raw("t", "{}", 0, 21)).await);
    // The error mapping never leaks SQLite text.
    let e = sqlx::query(&raw("t", "{}", 0, 99))
        .execute(s.pool())
        .await
        .unwrap_err();
    assert!(matches!(rusty_store::map_db_error(e), Error::Invalid(m) if m == "invalid value"));
}
