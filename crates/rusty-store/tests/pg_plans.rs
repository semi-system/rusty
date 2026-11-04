//! Plan-shape regression tests for the hot Postgres statements (review PG5 I1, I3).
//!
//! The statements are explained with `enable_seqscan = off` on a small (empty) schema: a plan
//! that still needs a sequential scan then has NO index it can use, which does not depend on
//! table statistics or row counts. A separate scale test (`claim_plan_at_scale`) checks the
//! claim statement on 100k rows with the planner left alone.
//! Requires `RUSTY_TEST_PG_URL` (loud SKIPPED notice otherwise).
//! backend-neutral: Postgres-only suite (needs a server; compares against the SQLite behaviour elsewhere)
#![cfg(feature = "postgres")]

mod common;

use common::pg::{pg_admin_url, PgTestDb};
use sqlx::{Connection, PgConnection};

/// The text of `EXPLAIN <sql>` with sequential scans disabled.
async fn plan_without_seqscan(conn: &mut PgConnection, sql: &str) -> String {
    sqlx::query("SET enable_seqscan = off").execute(&mut *conn).await.unwrap();
    let rows: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {sql}"))
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    rows.join("\n")
}

/// The foreign key `messages.blob_hash -> blobs.hash ON DELETE RESTRICT` makes Postgres run
/// exactly this probe for every `blobs` row GC deletes (`ri_triggers.c`, RESTRICT/NO ACTION
/// check). Without an index on `messages(blob_hash)` it is a sequential scan of `messages`:
/// 5.5 s per `GC_BATCH` of 1000 rows at 100k messages, versus 17 ms with the index.
#[tokio::test]
async fn gc_delete_foreign_key_probe_uses_the_blob_hash_index() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;
    let mut conn = PgConnection::connect(db.url()).await.unwrap();
    let plan = plan_without_seqscan(
        &mut conn,
        "SELECT 1 FROM ONLY messages x WHERE blob_hash = decode(repeat('aa',32),'hex') FOR KEY SHARE OF x",
    )
    .await;
    assert!(
        !plan.contains("Seq Scan"),
        "the RESTRICT probe on messages(blob_hash) has no index to use:\n{plan}"
    );
    assert!(plan.contains("messages_blob_hash"), "expected the messages_blob_hash index:\n{plan}");
    conn.close().await.unwrap();
    store.close().await;
    db.cleanup().await;
}

/// 100k-row job table in the shape the review measured: mostly finished rows, a large queued
/// backlog (many with the same `run_after` second), and a few running rows (live and expired).
async fn seed_jobs(conn: &mut PgConnection) {
    sqlx::query(
        "INSERT INTO jobs (kind, state, payload, attempts, max_attempts, run_after, created_at, updated_at) \
         SELECT CASE WHEN i % 4 = 0 THEN 'rare' ELSE 'common' END, 'done', '{}', 1, 5, i / 10, 0, 0 \
         FROM generate_series(1, 100000) i",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs (kind, state, payload, attempts, max_attempts, run_after, created_at, updated_at) \
         SELECT CASE WHEN i % 4 = 0 THEN 'rare' ELSE 'common' END, 'queued', '{}', 0, 5, 1000000 + i / 50, 0, 0 \
         FROM generate_series(1, 100000) i",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs (kind, state, payload, attempts, max_attempts, run_after, lease_owner, \
                           lease_expires_at, epoch, created_at, updated_at) \
         SELECT 'common', 'running', '{}', 1, 5, 500000 + i, 'w', \
                CASE WHEN i <= 20 THEN 1000 ELSE 9000000000 END, 1, 0, 0 \
         FROM generate_series(1, 2000) i",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("ANALYZE jobs").execute(&mut *conn).await.unwrap();
}

/// Review PG5 I3: the claim probe must be ordered index probes, never a scan of the job table.
/// The statements are the production texts (`claim_statements_for_test`), explained as a
/// generic plan (parameters unknown, which is the worst case for the planner) and with
/// concrete values, on 202,000 rows with the planner left alone.
#[tokio::test]
async fn claim_probes_use_index_scans_at_scale() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let mut conn = PgConnection::connect(db.url()).await.unwrap();
    // Migrations run on connect of a PgStore.
    let store = db.connect().await;
    seed_jobs(&mut conn).await;
    let (peek, lock) = rusty_store::repo::pg::claim_statements_for_test();

    let generic = explain_raw(&mut conn, &format!("EXPLAIN (GENERIC_PLAN) {peek}")).await;
    let concrete_sql = peek
        .replace("$1", "ARRAY['common']")
        .replace("$2", "NULL::bigint")
        .replace("$3", "-9223372036854775807")
        .replace("$4", "-9223372036854775807");
    let concrete: Vec<String> =
        sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, BUFFERS OFF, TIMING OFF) {concrete_sql}"))
            .fetch_all(&mut conn)
            .await
            .unwrap();
    let concrete = concrete.join("\n");
    for (what, plan) in [("generic", &generic), ("concrete", &concrete)] {
        assert!(!plan.contains("Seq Scan on jobs"), "{what} claim probe scans jobs:\n{plan}");
        assert!(!plan.contains("Bitmap"), "{what} claim probe is a bitmap scan:\n{plan}");
        assert!(plan.contains("jobs_queued_order"), "{what}: queued arm misses its index:\n{plan}");
        assert!(plan.contains("jobs_running_lease"), "{what}: running arm misses its index:\n{plan}");
    }
    // The arms stop at their LIMIT: they read about a window of rows, not the backlog.
    for line in concrete.lines().filter(|l| l.contains("Rows Removed by Filter")) {
        let n: u64 = line.rsplit(':').next().unwrap().trim().parse().unwrap();
        assert!(n <= 200, "a probe reads and discards {n} rows:
{concrete}");
    }

    // The lock statement is a primary-key probe over the window ids.
    let lock_plan = explain_raw(&mut conn, &format!("EXPLAIN (GENERIC_PLAN) {lock}")).await;
    assert!(!lock_plan.contains("Seq Scan on jobs"), "lock statement scans jobs:\n{lock_plan}");

    conn.close().await.unwrap();
    store.close().await;
    db.cleanup().await;
}

/// `EXPLAIN` through the simple query protocol (a generic plan takes no bind values).
async fn explain_raw(conn: &mut PgConnection, sql: &str) -> String {
    use sqlx::{Executor, Row};
    let rows = (&mut *conn).fetch_all(sqlx::raw_sql(sql)).await.unwrap();
    rows.iter().map(|r| r.get::<String, _>(0)).collect::<Vec<_>>().join("\n")
}
