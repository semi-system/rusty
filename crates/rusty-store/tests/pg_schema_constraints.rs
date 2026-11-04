//! backend-neutral: Postgres-only suite (needs a server; compares against the SQLite behaviour elsewhere)
#![cfg(feature = "postgres")]

mod common;

use common::pg::{pg_admin_url, PgTestDb};
use rusty_core::{BlobHash, Error};
use rusty_store::{map_db_error, PgStore};
use sqlx::PgPool;

struct Seed {
    domain: i64,
    account_a: i64,
}

async fn database() -> Option<(PgTestDb, PgStore)> {
    let url = pg_admin_url()?;
    let db = PgTestDb::create(&url).await;
    let store = db.connect().await;
    Some((db, store))
}

async fn seed(pool: &PgPool) -> Seed {
    let domain: i64 = sqlx::query_scalar(
        "INSERT INTO domains(name,created_at) VALUES ('example.com',0) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let account_a: i64 = sqlx::query_scalar(
        "INSERT INTO accounts(local_part,domain_id,status,created_at) \
         VALUES ('a',$1,'active',0) RETURNING id",
    )
    .bind(domain)
    .fetch_one(pool)
    .await
    .unwrap();
    let account_b: i64 = sqlx::query_scalar(
        "INSERT INTO accounts(local_part,domain_id,status,created_at) \
         VALUES ('b',$1,'active',0) RETURNING id",
    )
    .bind(domain)
    .fetch_one(pool)
    .await
    .unwrap();
    let mailbox_a: i64 = sqlx::query_scalar(
        "INSERT INTO mailboxes(account_id,uid_validity) VALUES ($1,1) RETURNING id",
    )
    .bind(account_a)
    .fetch_one(pool)
    .await
    .unwrap();
    let mailbox_b: i64 = sqlx::query_scalar(
        "INSERT INTO mailboxes(account_id,uid_validity) VALUES ($1,1) RETURNING id",
    )
    .bind(account_b)
    .fetch_one(pool)
    .await
    .unwrap();
    for mailbox in [mailbox_a, mailbox_b] {
        sqlx::query(
            "INSERT INTO folders(mailbox_id,id,parent_id,name,role,uid_validity) \
             VALUES ($1,1,NULL,'Inbox','inbox',1)",
        )
        .bind(mailbox)
        .execute(pool)
        .await
        .unwrap();
    }
    for byte in [0xaau8, 0xbbu8] {
        sqlx::query("INSERT INTO blobs(hash,size) VALUES ($1,10)")
            .bind(vec![byte; 32])
            .execute(pool)
            .await
            .unwrap();
    }
    Seed {
        domain,
        account_a,
    }
}

#[tokio::test]
async fn pg_bytea_blob_hash_round_trips_without_changing_public_hex_type() {
    let Some((db, store)) = database().await else {
        return;
    };
    let hash = BlobHash::of(b"postgres bytea mapping");
    let bytes = hex::decode(hash.as_str()).unwrap();
    sqlx::query("INSERT INTO blobs(hash,size) VALUES ($1,1)")
        .bind(&bytes)
        .execute(store.pool())
        .await
        .unwrap();
    let fetched: Vec<u8> = sqlx::query_scalar("SELECT hash FROM blobs WHERE hash=$1")
        .bind(&bytes)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(BlobHash::parse(&hex::encode(fetched)).unwrap(), hash);
    store.close().await;
    db.cleanup().await;
}

async fn bad(pool: &PgPool, sql: &str) -> sqlx::Error {
    sqlx::query(sql).execute(pool).await.unwrap_err()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_address_uniqueness_is_concurrency_safe() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    for n in 0..12 {
        let local = format!("race{n}");
        let q1 =
            sqlx::query("INSERT INTO aliases(local_part,domain_id,account_id) VALUES ($1,$2,$3)")
                .bind(&local)
                .bind(s.domain)
                .bind(s.account_a)
                .execute(p);
        let q2 = sqlx::query("INSERT INTO lists(local_part,domain_id) VALUES ($1,$2)")
            .bind(&local)
            .bind(s.domain)
            .execute(p);
        let (a, b) = tokio::join!(q1, q2);
        assert_ne!(a.is_ok(), b.is_ok(), "both results matched for {local}");
        let owners: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM aliases WHERE local_part=$1 AND domain_id=$2) + \
                    (SELECT count(*) FROM lists WHERE local_part=$1 AND domain_id=$2)",
        )
        .bind(&local)
        .bind(s.domain)
        .fetch_one(p)
        .await
        .unwrap();
        assert_eq!(owners, 1);
    }
    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn pg_error_mapping_uses_sqlstate_not_message_text() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    let check = bad(
        p,
        "INSERT INTO domains(name,created_at) VALUES ('RS001-UPPER',0)",
    )
    .await;
    assert!(matches!(map_db_error(check), Error::Invalid(_)));

    sqlx::query("INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('used',$1,$2)")
        .bind(s.domain)
        .bind(s.account_a)
        .execute(p)
        .await
        .unwrap();
    let custom = sqlx::query("INSERT INTO lists(local_part,domain_id) VALUES ('used',$1)")
        .bind(s.domain)
        .execute(p)
        .await
        .unwrap_err();
    assert_eq!(
        custom.as_database_error().unwrap().code().as_deref(),
        Some("RS001")
    );
    assert!(matches!(map_db_error(custom), Error::Conflict(_)));

    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn pg_dropping_address_trigger_demonstrates_negative_test_sensitivity() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    sqlx::query("INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('mutant',$1,$2)")
        .bind(s.domain)
        .bind(s.account_a)
        .execute(p)
        .await
        .unwrap();
    sqlx::query("DROP TRIGGER lists_addr_unique_ins ON lists")
        .execute(p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO lists(local_part,domain_id) VALUES ('mutant',$1)")
        .bind(s.domain)
        .execute(p)
        .await
        .expect("the forbidden state must become possible after the guarding trigger is removed");
    store.close().await;
    db.cleanup().await;
}
