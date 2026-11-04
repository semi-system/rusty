//! Schema tests for migration 0007 (`capabilities`, `accounts_id_domain`).
//! backend-neutral: SQLite schema constraints; the Postgres twins (one per test, exact constraint) are listed in pg_schema_constraints.rs PORTED

mod common;

use common::mem;
use rusty_store::SqliteStore;
use sqlx::SqlitePool;

async fn ok(p: &SqlitePool, sql: &str) {
    if let Err(e) = sqlx::query(sql).execute(p).await {
        panic!("expected success for `{sql}`, got {e}");
    }
}

async fn bad(p: &SqlitePool, sql: &str) {
    assert!(
        sqlx::query(sql).execute(p).await.is_err(),
        "expected failure for `{sql}`"
    );
}

async fn count(p: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(p).await.unwrap()
}

/// Domains 1 and 2, account 1 in domain 1.
async fn seed() -> SqliteStore {
    let s = mem().await;
    let p = s.pool();
    ok(
        p,
        "INSERT INTO domains(id,name,created_at) VALUES (1,'a.org',0)",
    )
    .await;
    ok(
        p,
        "INSERT INTO domains(id,name,created_at) VALUES (2,'b.org',0)",
    )
    .await;
    ok(
        p,
        "INSERT INTO accounts(id,local_part,domain_id,status,created_at) VALUES (1,'a',1,'active',0)",
    )
    .await;
    s
}

fn cap(
    hash_byte: &str,
    account: i64,
    domain: i64,
    role: &str,
    issued: i64,
    expires: i64,
) -> String {
    format!(
        "INSERT INTO capabilities(token_hash,account_id,domain_id,role,issued_at,expires_at) \
         VALUES (x'00000000000000000000000000000000000000000000000000000000000000{hash_byte}', {account}, {domain}, '{role}', {issued}, {expires})"
    )
}

#[tokio::test]
async fn capabilities_table_accepts_valid_row() {
    let s = seed().await;
    let p = s.pool();
    ok(
        p,
        "INSERT INTO capabilities(token_hash,account_id,domain_id,role,issued_at,expires_at) \
         VALUES (zeroblob(32), 1, 1, 'imap', 100, 200)",
    )
    .await;
    assert_eq!(count(p, "SELECT count(*) FROM capabilities").await, 1);
}

#[tokio::test]
async fn capability_with_mismatched_domain_is_rejected() {
    let s = seed().await;
    let p = s.pool();
    // Account 1 lives in domain 1, not 2.
    bad(
        p,
        "INSERT INTO capabilities(token_hash,account_id,domain_id,role,issued_at,expires_at) \
         VALUES (zeroblob(32), 1, 2, 'imap', 100, 200)",
    )
    .await;
    // Nonexistent account.
    bad(
        p,
        "INSERT INTO capabilities(token_hash,account_id,domain_id,role,issued_at,expires_at) \
         VALUES (zeroblob(32), 9, 1, 'imap', 100, 200)",
    )
    .await;
    assert_eq!(count(p, "SELECT count(*) FROM capabilities").await, 0);
}

#[tokio::test]
async fn capability_row_constraints() {
    let s = seed().await;
    let p = s.pool();
    // hash length 31 / 33 / text
    bad(
        p,
        "INSERT INTO capabilities(token_hash,account_id,domain_id,role,issued_at,expires_at) \
         VALUES (zeroblob(31), 1, 1, 'imap', 100, 200)",
    )
    .await;
    bad(
        p,
        "INSERT INTO capabilities(token_hash,account_id,domain_id,role,issued_at,expires_at) \
         VALUES (zeroblob(33), 1, 1, 'imap', 100, 200)",
    )
    .await;
    bad(
        p,
        "INSERT INTO capabilities(token_hash,account_id,domain_id,role,issued_at,expires_at) \
         VALUES ('0123456789abcdef0123456789abcdef', 1, 1, 'imap', 100, 200)",
    )
    .await;
    // role
    bad(p, &cap("01", 1, 1, "admin", 100, 200)).await;
    bad(p, &cap("01", 1, 1, "IMAP", 100, 200)).await;
    for (i, r) in ["imap", "jmap", "dav", "smtp-submit"].iter().enumerate() {
        ok(p, &cap(&format!("{:02x}", i + 1), 1, 1, r, 100, 200)).await;
    }
    // expiry window: expires_at > issued_at, lifetime <= 86400
    bad(p, &cap("10", 1, 1, "imap", 100, 100)).await;
    bad(p, &cap("10", 1, 1, "imap", 100, 99)).await;
    bad(p, &cap("10", 1, 1, "imap", 100, 100 + 86401)).await;
    ok(p, &cap("10", 1, 1, "imap", 100, 100 + 86400)).await;
    // revoked_at, when set, is not before issuance
    bad(
        p,
        "UPDATE capabilities SET revoked_at = 50 WHERE token_hash = x'0000000000000000000000000000000000000000000000000000000000000010'",
    )
    .await;
    ok(
        p,
        "UPDATE capabilities SET revoked_at = 150 WHERE token_hash = x'0000000000000000000000000000000000000000000000000000000000000010'",
    )
    .await;
    // duplicate hash
    bad(p, &cap("10", 1, 1, "imap", 100, 200)).await;
}

#[tokio::test]
async fn deleting_account_cascades_to_capabilities() {
    let s = seed().await;
    let p = s.pool();
    ok(p, &cap("01", 1, 1, "imap", 100, 200)).await;
    ok(p, "DELETE FROM accounts WHERE id = 1").await;
    assert_eq!(count(p, "SELECT count(*) FROM capabilities").await, 0);
}

#[tokio::test]
async fn accounts_id_domain_unique_index_exists() {
    let s = seed().await;
    let n = count(
        s.pool(),
        "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='accounts_id_domain'",
    )
    .await;
    assert_eq!(n, 1);
}
