//! Plan-shape regression tests for the hot Postgres statements (review PG5 I1, I3).
//!
//! The statements are explained with `enable_seqscan = off` on a small (empty) schema: a plan
//! that still needs a sequential scan then has NO index it can use, which does not depend on
//! table statistics or row counts. A separate scale test (`claim_plan_at_scale`) checks the
//! claim statement on 100k rows with the planner left alone.
//! Requires `RUSTY_TEST_PG_URL` (loud SKIPPED notice otherwise).
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
