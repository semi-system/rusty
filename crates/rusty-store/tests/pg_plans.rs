//! Plan-shape regression tests for the hot Postgres statements (review PG5 I1, I3).
//!
//! The statements are explained with `enable_seqscan = off` on a small (empty) schema: a plan
//! that still needs a sequential scan then has NO index it can use, which does not depend on
//! table statistics or row counts. A separate scale test (`claim_plan_at_scale`) checks the
//! claim statement on 100k rows with the planner left alone.
//! Requires `RUSTY_TEST_PG_URL` (loud SKIPPED notice otherwise).

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
// backend-neutral: Postgres EXPLAIN plan shapes and index use; SQLite plans are not a contract of this crate
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
// backend-neutral: Postgres EXPLAIN plan shapes and index use; SQLite plans are not a contract of this crate
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

// ---------------------------------------------------------------------------------------------
// Secondary indexes (review PG6 finding 5): plans at scale for the queries that use them.
//
// The statements below repeat the production predicates with literal values.
// `statement_text_is_production_text` fails when a production source no longer contains the
// predicate, so a test cannot silently explain a statement nobody runs. `messages_thread` has no
// test because no statement of the codebase filters on `(mailbox_id, thread_id)` (threads are
// only READ back as a column); it is pinned by the catalog snapshot and the sqlite_master oracle
// only.

/// 100k messages over 50 mailboxes (1% soft-deleted), deliveries for 20% of them, 20k
/// capabilities of 50 accounts (some expired, some revoked), 100k jobs (2% of one domain).
async fn seed_scale(conn: &mut PgConnection) {
    for sql in [
        // The refcount and address triggers are irrelevant to plans; skipping them keeps seeding fast.
        "ALTER TABLE messages DISABLE TRIGGER messages_refcount_ins",
        "ALTER TABLE accounts DISABLE TRIGGER accounts_addr_unique_ins",
        "INSERT INTO domains(name, created_at) VALUES ('scale.test', 0)",
        "INSERT INTO accounts(local_part, domain_id, status, created_at) \
         SELECT 'u' || i, 1, 'active', 0 FROM generate_series(1, 50) i",
        "INSERT INTO mailboxes(account_id, uid_validity) SELECT id, 1 FROM accounts",
        "INSERT INTO folders(mailbox_id, id, parent_id, name, role, uid_validity) \
         SELECT id, 1, NULL, 'Inbox', 'inbox', 1 FROM mailboxes",
        "INSERT INTO blobs(hash, size, refcount) VALUES (decode(repeat('ab', 32), 'hex'), 10, 1)",
        "INSERT INTO messages(mailbox_id, id, folder_id, uid, blob_hash, size, internal_date, thread_id, \
                              message_id_hdr, deleted_at) \
         SELECT 1 + i % 50, i, 1, i, decode(repeat('ab', 32), 'hex'), 10, i, i % 1000, 'h' || i, \
                CASE WHEN i % 100 = 0 THEN i ELSE NULL END \
         FROM generate_series(1, 100000) i",
        "INSERT INTO deliveries(mailbox_id, delivery_id, message_id, blob_hash, created_at) \
         SELECT 1 + i % 50, decode(lpad(to_hex(i), 64, '0'), 'hex'), i, decode(repeat('ab', 32), 'hex'), i \
         FROM generate_series(5, 100000, 5) i",
        "INSERT INTO capabilities(token_hash, account_id, domain_id, role, issued_at, expires_at, revoked_at) \
         SELECT decode(lpad(to_hex(i), 64, '0'), 'hex'), 1 + i % 50, 1, 'imap', 1000, \
                CASE WHEN i % 50 = 0 THEN 1010 ELSE 80000 END, CASE WHEN i % 97 = 0 THEN 1005 END \
         FROM generate_series(1, 20000) i",
        "INSERT INTO jobs(kind, domain_id, state, payload, attempts, max_attempts, run_after, created_at, updated_at) \
         SELECT 'k', CASE WHEN i % 50 = 0 THEN 1 END, CASE WHEN i % 20 = 0 THEN 'queued' ELSE 'done' END, \
                '{}', 0, 5, i, 0, 0 FROM generate_series(1, 100000) i",
        "ANALYZE",
    ] {
        sqlx::query(sql).execute(&mut *conn).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// `EXPLAIN` of `sql` with the planner left alone, statistics from `seed_scale`.
async fn plan(conn: &mut PgConnection, sql: &str) -> String {
    let rows: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {sql}"))
        .fetch_all(&mut *conn)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows.join("\n")
}

fn uses(plan: &str, index: &str, what: &str) {
    assert!(plan.contains(index), "{what}: expected index {index}:\n{plan}");
}

fn production_source() -> String {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/repo/pg");
    let mut all = String::new();
    for f in ["mailbox.rs", "gc.rs", "jobs.rs", "directory.rs"] {
        all.push_str(&std::fs::read_to_string(dir.join(f)).unwrap());
    }
    // A string continuation (backslash, newline, indentation) joins the pieces.
    let mut out = String::new();
    let mut it = all.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' && it.peek() == Some(&'\n') {
            it.next();
            while it.peek() == Some(&' ') {
                it.next();
            }
            continue;
        }
        out.push(c);
    }
    out
}

// backend-neutral: reads sources only, no database
#[test]
fn statement_text_is_production_text() {
    let src = production_source();
    for fragment in [
        "WHERE deleted_at IS NOT NULL AND deleted_at < $1 AND mailbox_id > $3 AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.mailbox_id = m.mailbox_id AND d.message_id = m.id) ORDER BY mailbox_id LIMIT $2",
        "WHERE mailbox_id = $1 AND deleted_at IS NOT NULL AND deleted_at < $2 AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.mailbox_id = messages.mailbox_id AND d.message_id = messages.id) ORDER BY id LIMIT $3",
        "SELECT message_id_hdr, thread_id FROM messages WHERE mailbox_id = $1 AND id IN ( SELECT MIN(id) FROM messages WHERE mailbox_id = $1 AND message_id_hdr = ANY($2) GROUP BY message_id_hdr)",
        "SELECT mailbox_id, delivery_id FROM deliveries WHERE created_at < $1 ORDER BY created_at LIMIT $2 FOR UPDATE SKIP LOCKED",
        "WHERE expires_at < $1 OR revoked_at < $1 ORDER BY token_hash LIMIT $2 FOR UPDATE SKIP LOCKED",
        "WHERE account_id = $2 AND revoked_at IS NULL AND expires_at > $1 AND token_hash <> $4 ORDER BY issued_at, token_hash",
        "SELECT count(*) FROM jobs WHERE domain_id = $1 AND state IN ('queued', 'running')",
    ] {
        assert!(src.contains(fragment), "the production source no longer contains: {fragment}");
    }
}

// backend-neutral: Postgres EXPLAIN plan shapes and index use; SQLite plans are not a contract of this crate
#[tokio::test]
async fn message_indexes_serve_the_purge_and_the_thread_lookup() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;
    let mut conn = PgConnection::connect(db.url()).await.unwrap();
    seed_scale(&mut conn).await;
    // GC purge: mailboxes that have something purgeable (soft-deleted rows are 1%).
    let p = plan(
        &mut conn,
        "SELECT DISTINCT mailbox_id FROM messages m WHERE deleted_at IS NOT NULL AND deleted_at < 50000 \
         AND mailbox_id > 0 AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.mailbox_id = m.mailbox_id \
         AND d.message_id = m.id) ORDER BY mailbox_id LIMIT 100",
    )
    .await;
    uses(&p, "messages_deleted_at", "purge candidates");
    uses(&p, "deliveries_message", "purge candidates anti-join");
    // Per-mailbox purge batch.
    let p = plan(
        &mut conn,
        "SELECT id, blob_hash FROM messages WHERE mailbox_id = 10 AND deleted_at IS NOT NULL AND deleted_at < 50000 \
         AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.mailbox_id = messages.mailbox_id \
         AND d.message_id = messages.id) ORDER BY id LIMIT 1000",
    )
    .await;
    uses(&p, "messages_deleted_at", "per-mailbox purge");
    // Thread lookup by Message-ID header.
    let p = plan(
        &mut conn,
        "SELECT message_id_hdr, thread_id FROM messages WHERE mailbox_id = 10 AND id IN ( \
         SELECT MIN(id) FROM messages WHERE mailbox_id = 10 AND message_id_hdr = ANY(ARRAY['h10','h60']) \
         GROUP BY message_id_hdr)",
    )
    .await;
    uses(&p, "messages_msgid", "thread lookup by Message-ID");
    conn.close().await.unwrap();
    store.close().await;
    db.cleanup().await;
}

// backend-neutral: Postgres EXPLAIN plan shapes and index use; SQLite plans are not a contract of this crate
#[tokio::test]
async fn delivery_indexes_serve_the_purge_and_the_cascade() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;
    let mut conn = PgConnection::connect(db.url()).await.unwrap();
    seed_scale(&mut conn).await;
    let p = plan(
        &mut conn,
        "SELECT mailbox_id, delivery_id FROM deliveries WHERE created_at < 500 ORDER BY created_at LIMIT 1000 \
         FOR UPDATE SKIP LOCKED",
    )
    .await;
    uses(&p, "deliveries_created", "delivery purge by age");
    assert!(!p.contains("Sort"), "delivery purge sorts instead of reading the index in order:\n{p}");
    // Deleting a message cascades to its delivery row through this probe.
    let p = plan(
        &mut conn,
        "SELECT 1 FROM ONLY deliveries x WHERE mailbox_id = 10 AND message_id = 60 FOR KEY SHARE OF x",
    )
    .await;
    uses(&p, "deliveries_message", "message delete cascade probe");
    conn.close().await.unwrap();
    store.close().await;
    db.cleanup().await;
}

// backend-neutral: Postgres EXPLAIN plan shapes and index use; SQLite plans are not a contract of this crate
#[tokio::test]
async fn capability_indexes_serve_revoke_purge_and_the_account_cascade() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;
    let mut conn = PgConnection::connect(db.url()).await.unwrap();
    seed_scale(&mut conn).await;
    let p = plan(
        &mut conn,
        "SELECT token_hash FROM capabilities WHERE account_id = 7 AND revoked_at IS NULL AND expires_at > 2000 \
         AND token_hash <> decode(repeat('00', 32), 'hex') ORDER BY issued_at, token_hash LIMIT 3",
    )
    .await;
    uses(&p, "capabilities_account", "revoke of the oldest capabilities of an account");
    let p = plan(
        &mut conn,
        "SELECT 1 FROM ONLY capabilities x WHERE account_id = 7 AND domain_id = 1 FOR KEY SHARE OF x",
    )
    .await;
    uses(&p, "capabilities_account", "account delete cascade probe");
    let p = plan(
        &mut conn,
        "SELECT token_hash FROM capabilities WHERE expires_at < 1500 OR revoked_at < 1500 \
         ORDER BY token_hash LIMIT 1000 FOR UPDATE SKIP LOCKED",
    )
    .await;
    uses(&p, "capabilities_expires", "capability purge, expired arm");
    uses(&p, "capabilities_revoked", "capability purge, revoked arm");
    conn.close().await.unwrap();
    store.close().await;
    db.cleanup().await;
}

// backend-neutral: Postgres EXPLAIN plan shapes and index use; SQLite plans are not a contract of this crate
#[tokio::test]
async fn job_domain_index_serves_the_enqueue_cap_count() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;
    let mut conn = PgConnection::connect(db.url()).await.unwrap();
    seed_scale(&mut conn).await;
    let p = plan(
        &mut conn,
        "SELECT count(*) FROM jobs WHERE domain_id = 1 AND state IN ('queued', 'running')",
    )
    .await;
    uses(&p, "jobs_domain_state", "per-domain pending count");
    let p = plan(&mut conn, "SELECT 1 FROM ONLY jobs x WHERE domain_id = 1 FOR KEY SHARE OF x").await;
    uses(&p, "jobs_domain_state", "domain delete RESTRICT probe");
    conn.close().await.unwrap();
    store.close().await;
    db.cleanup().await;
}
