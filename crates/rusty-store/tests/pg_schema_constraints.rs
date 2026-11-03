#![cfg(feature = "postgres")]

mod common;

use common::pg::{pg_admin_url, PgTestDb};
use rusty_core::{BlobHash, Error};
use rusty_store::{map_db_error, PgStore};
use sqlx::PgPool;

struct Seed {
    domain: i64,
    account_a: i64,
    mailbox_a: i64,
    mailbox_b: i64,
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
        mailbox_a,
        mailbox_b,
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

#[tokio::test]
async fn pg_core_constraints_reject_invalid_rows() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;

    for sql in [
        "INSERT INTO domains(name,created_at) VALUES ('UP.example',0)",
        "INSERT INTO domains(name,config,created_at) VALUES ('bad.example','{',0)",
        "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('x',999999,'active',0)",
        "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('x',1,'bogus',0)",
        "INSERT INTO blobs(hash,size) VALUES ('abc'::bytea,1)",
        "INSERT INTO blobs(hash,size,refcount) VALUES (decode(repeat('cc',32),'hex'),1,-1)",
        "INSERT INTO jobs(kind,payload,run_after,created_at,updated_at) VALUES ('UP','{}',0,0,0)",
        "INSERT INTO jobs(kind,payload,run_after,created_at,updated_at) VALUES ('ok','{',0,0,0)",
        "INSERT INTO jobs(kind,payload,attempts,max_attempts,run_after,created_at,updated_at) VALUES ('ok','{}',2,1,0,0,0)",
        "INSERT INTO jobs(kind,payload,state,run_after,created_at,updated_at) VALUES ('ok','{}','running',0,0,0)",
    ] {
        assert!(sqlx::query(sql).execute(p).await.is_err(), "accepted: {sql}");
    }

    let list: i64 = sqlx::query_scalar(
        "INSERT INTO lists(local_part,domain_id) VALUES ('team',$1) RETURNING id",
    )
    .bind(s.domain)
    .fetch_one(p)
    .await
    .unwrap();
    assert!(sqlx::query(
        "INSERT INTO list_members(list_id,account_id,external) VALUES ($1,$2,'x@y.test')",
    )
    .bind(list)
    .bind(s.account_a)
    .execute(p)
    .await
    .is_err());
    assert!(sqlx::query("INSERT INTO list_members(list_id) VALUES ($1)")
        .bind(list)
        .execute(p)
        .await
        .is_err());

    sqlx::query(
        "INSERT INTO folders(mailbox_id,id,parent_id,name,role,uid_validity) \
         VALUES ($1,2,1,'Work','custom',2)",
    )
    .bind(s.mailbox_a)
    .execute(p)
    .await
    .unwrap();
    assert!(sqlx::query(
        "INSERT INTO folders(mailbox_id,id,parent_id,name,role,uid_validity) \
             VALUES ($1,3,1,'Work','custom',3)",
    )
    .bind(s.mailbox_a)
    .execute(p)
    .await
    .is_err());
    assert!(sqlx::query(
        "INSERT INTO folders(mailbox_id,id,parent_id,name,role,uid_validity) \
             VALUES ($1,3,3,'Loop','custom',3)",
    )
    .bind(s.mailbox_a)
    .execute(p)
    .await
    .is_err());

    assert!(
        sqlx::query(
            "INSERT INTO messages(mailbox_id,id,folder_id,uid,blob_hash,size,internal_date,thread_id,flags) \
             VALUES ($1,1,1,1,$2,10,0,1,32)",
        )
        .bind(s.mailbox_a)
        .bind(vec![0xaau8; 32])
        .execute(p)
        .await
        .is_err()
    );
    assert!(sqlx::query(
        "INSERT INTO messages(mailbox_id,id,folder_id,uid,blob_hash,size,internal_date,thread_id) \
             VALUES ($1,1,1,1,$2,10,0,1)",
    )
    .bind(s.mailbox_b)
    .bind(vec![0xaau8; 32])
    .execute(p)
    .await
    .is_ok());
    assert!(sqlx::query(
        "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES ($1,1,'Upper')",
    )
    .bind(s.mailbox_b)
    .execute(p)
    .await
    .is_err());

    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn pg_deliveries_jobs_and_capabilities_constraints() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    sqlx::query(
        "INSERT INTO messages(mailbox_id,id,folder_id,uid,blob_hash,size,internal_date,thread_id) \
         VALUES ($1,1,1,1,$2,10,0,1)",
    )
    .bind(s.mailbox_a)
    .bind(vec![0xaau8; 32])
    .execute(p)
    .await
    .unwrap();

    for sql in [
        format!(
            "INSERT INTO deliveries(mailbox_id,delivery_id,message_id,blob_hash,created_at) \
             VALUES ({},'x'::bytea,1,decode(repeat('aa',32),'hex'),0)",
            s.mailbox_a
        ),
        format!(
            "INSERT INTO deliveries(mailbox_id,delivery_id,message_id,blob_hash,created_at) \
             VALUES ({},decode(repeat('11',32),'hex'),999,decode(repeat('aa',32),'hex'),0)",
            s.mailbox_a
        ),
        format!(
            "INSERT INTO capabilities(token_hash,account_id,domain_id,role,issued_at,expires_at) \
             VALUES (decode(repeat('11',32),'hex'),{},{},'root',0,10)",
            s.account_a, s.domain
        ),
        format!(
            "INSERT INTO capabilities(token_hash,account_id,domain_id,role,issued_at,expires_at) \
             VALUES (decode(repeat('22',32),'hex'),{},{},'imap',10,10)",
            s.account_a, s.domain
        ),
    ] {
        assert!(
            sqlx::query(&sql).execute(p).await.is_err(),
            "accepted: {sql}"
        );
    }

    let huge = serde_json::to_string(&"x".repeat(65_536)).unwrap();
    assert!(huge.len() > 65_536);
    assert!(sqlx::query(
        "INSERT INTO jobs(kind,payload,run_after,created_at,updated_at) VALUES ('large',$1,0,0,0)",
    )
    .bind(huge)
    .execute(p)
    .await
    .is_err());

    sqlx::query(
        "INSERT INTO deliveries(mailbox_id,delivery_id,message_id,blob_hash,created_at) \
         VALUES ($1,$2,1,$3,0)",
    )
    .bind(s.mailbox_a)
    .bind(vec![0x11u8; 32])
    .bind(vec![0xaau8; 32])
    .execute(p)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO capabilities(token_hash,account_id,domain_id,role,issued_at,expires_at) \
         VALUES ($1,$2,$3,'imap',0,10)",
    )
    .bind(vec![0x33u8; 32])
    .bind(s.account_a)
    .bind(s.domain)
    .execute(p)
    .await
    .unwrap();

    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn pg_refcount_zero_since_and_folder_cycle_triggers() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    let a = vec![0xaau8; 32];
    let b = vec![0xbbu8; 32];
    let before: Option<i64> = sqlx::query_scalar("SELECT zero_since FROM blobs WHERE hash=$1")
        .bind(&a)
        .fetch_one(p)
        .await
        .unwrap();
    assert!(before.is_some());
    sqlx::query(
        "INSERT INTO messages(mailbox_id,id,folder_id,uid,blob_hash,size,internal_date,thread_id) \
         VALUES ($1,1,1,1,$2,10,0,1)",
    )
    .bind(s.mailbox_a)
    .bind(&a)
    .execute(p)
    .await
    .unwrap();
    let state: (i64, Option<i64>) =
        sqlx::query_as("SELECT refcount,zero_since FROM blobs WHERE hash=$1")
            .bind(&a)
            .fetch_one(p)
            .await
            .unwrap();
    assert_eq!(state, (1, None));
    sqlx::query("UPDATE messages SET blob_hash=$1 WHERE mailbox_id=$2 AND id=1")
        .bind(&b)
        .bind(s.mailbox_a)
        .execute(p)
        .await
        .unwrap();
    let old: (i64, Option<i64>) =
        sqlx::query_as("SELECT refcount,zero_since FROM blobs WHERE hash=$1")
            .bind(&a)
            .fetch_one(p)
            .await
            .unwrap();
    let new: (i64, Option<i64>) =
        sqlx::query_as("SELECT refcount,zero_since FROM blobs WHERE hash=$1")
            .bind(&b)
            .fetch_one(p)
            .await
            .unwrap();
    assert_eq!(old.0, 0);
    assert!(old.1.is_some());
    assert_eq!(new, (1, None));

    sqlx::query(
        "INSERT INTO folders(mailbox_id,id,parent_id,name,role,uid_validity) VALUES ($1,2,1,'A','custom',2),($1,3,2,'B','custom',3)",
    )
    .bind(s.mailbox_a)
    .execute(p)
    .await
    .unwrap();
    let error = sqlx::query("UPDATE folders SET parent_id=3 WHERE mailbox_id=$1 AND id=1")
        .bind(s.mailbox_a)
        .execute(p)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("RS002")
    );

    store.close().await;
    db.cleanup().await;
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
async fn pg_address_registry_tracks_update_and_delete() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    let alias: i64 = sqlx::query_scalar(
        "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('old',$1,$2) RETURNING id",
    )
    .bind(s.domain)
    .bind(s.account_a)
    .fetch_one(p)
    .await
    .unwrap();
    sqlx::query("UPDATE aliases SET local_part='new' WHERE id=$1")
        .bind(alias)
        .execute(p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO lists(local_part,domain_id) VALUES ('old',$1)")
        .bind(s.domain)
        .execute(p)
        .await
        .expect("the old address must be released by update");
    assert!(
        sqlx::query("INSERT INTO lists(local_part,domain_id) VALUES ('new',$1)")
            .bind(s.domain)
            .execute(p)
            .await
            .is_err()
    );
    sqlx::query("DELETE FROM aliases WHERE id=$1")
        .bind(alias)
        .execute(p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO lists(local_part,domain_id) VALUES ('new',$1)")
        .bind(s.domain)
        .execute(p)
        .await
        .expect("delete must release the address");
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
