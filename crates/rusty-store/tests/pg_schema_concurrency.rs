//! Two-session tests for invariants that must hold under concurrent writers. SQLite's single
//! writer made these impossible; on Postgres the schema itself has to serialize them.
#![cfg(feature = "postgres")]

mod common;

use std::time::Duration;

use common::pg::{pg_admin_url, PgTestDb};
use sqlx::PgPool;

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|e| e.code().map(|c| c.into_owned()))
}

async fn mailbox_with_folders(pool: &PgPool, folders: i64) -> i64 {
    let domain: i64 = sqlx::query_scalar(
        "INSERT INTO domains(name,created_at) VALUES ('example.com',0) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let account: i64 = sqlx::query_scalar(
        "INSERT INTO accounts(local_part,domain_id,status,created_at) \
         VALUES ('a',$1,'active',0) RETURNING id",
    )
    .bind(domain)
    .fetch_one(pool)
    .await
    .unwrap();
    let mailbox: i64 = sqlx::query_scalar(
        "INSERT INTO mailboxes(account_id,uid_validity) VALUES ($1,1) RETURNING id",
    )
    .bind(account)
    .fetch_one(pool)
    .await
    .unwrap();
    for id in 1..=folders {
        sqlx::query(
            "INSERT INTO folders(mailbox_id,id,parent_id,name,role,uid_validity) \
             VALUES ($1,$2,NULL,$3,'custom',1)",
        )
        .bind(mailbox)
        .bind(id)
        .bind(format!("f{id}"))
        .execute(pool)
        .await
        .unwrap();
    }
    mailbox
}

/// T1 reparents 1 under 2 and stays open. T2 reparents 2 under 1 concurrently. Without the
/// mailbox lock inside the trigger both commit and 1<->2 is a cycle; with it T2 waits for T1,
/// then sees T1's committed row and is rejected with RS002.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reparenting_cannot_create_a_folder_cycle() {
    let Some(url) = pg_admin_url() else {
        return;
    };
    let db = PgTestDb::create(&url).await;
    let store = db.connect().await;
    let pool = store.pool();
    let mailbox = mailbox_with_folders(pool, 2).await;

    let mut t1 = pool.begin().await.unwrap();
    sqlx::query("UPDATE folders SET parent_id=2 WHERE mailbox_id=$1 AND id=1")
        .bind(mailbox)
        .execute(&mut *t1)
        .await
        .unwrap();

    let pool2 = pool.clone();
    let t2 = tokio::spawn(async move {
        let mut tx = pool2.begin().await.unwrap();
        let result = sqlx::query("UPDATE folders SET parent_id=1 WHERE mailbox_id=$1 AND id=2")
            .bind(mailbox)
            .execute(&mut *tx)
            .await;
        match result {
            Ok(_) => {
                tx.commit().await.unwrap();
                Ok(())
            }
            Err(e) => Err(e),
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    t1.commit().await.unwrap();

    let outcome = t2.await.unwrap();
    let error = outcome.expect_err("the second reparenting must be rejected, not committed");
    assert_eq!(sqlstate(&error).as_deref(), Some("RS002"), "{error}");

    let parents: Vec<(i64, Option<i64>)> =
        sqlx::query_as("SELECT id, parent_id FROM folders WHERE mailbox_id=$1 ORDER BY id")
            .bind(mailbox)
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(parents, vec![(1, Some(2)), (2, None)]);

    store.close().await;
    db.cleanup().await;
}
