//! The schema compatibility range (migration 0033, docs/design-zero-downtime.md): a binary runs on a schema that is newer than its own when every newer migration says it keeps the binary
//! working, and refuses one that is not declared compatible. A migration that was applied but said nothing is not compatible.

mod common;

use common::pg::{pg_admin_url, PgTestDb};
use rusty_store::PgStore;
use sqlx::postgres::PgPoolOptions;
use sqlx::Executor;

async fn run(pool: &sqlx::PgPool, sql: String) {
    pool.execute(sqlx::raw_sql(sqlx::AssertSqlSafe(sql))).await.unwrap();
}

fn applied(version: i64) -> String {
    format!("INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) VALUES ({version}, 'newer', now(), true, decode('00', 'hex'), 1)")
}

#[tokio::test]
async fn a_newer_schema_is_accepted_only_when_every_newer_migration_keeps_this_binary_working() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    // Migrations run on connect; this binary is at the schema it was built for.
    PgStore::connect(db.url()).await.unwrap().close().await;
    let pool = PgPoolOptions::new().max_connections(1).connect(db.url()).await.unwrap();
    let known: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations").fetch_one(&pool).await.unwrap();

    // A newer release applied a migration and said nothing about compatibility: this binary refuses.
    run(&pool, applied(known + 1)).await;
    assert!(PgStore::connect(db.url()).await.is_err(), "an undeclared newer migration stops the old binary");
    // It declares that a binary at schema `known` keeps working: now it runs.
    run(&pool, format!("INSERT INTO schema_compat (version, min_binary) VALUES ({}, {known})", known + 1)).await;
    PgStore::connect(db.url()).await.expect("a declared compatible newer schema is accepted").close().await;
    // A later migration that needs a newer binary stops it again.
    run(&pool, applied(known + 2)).await;
    run(&pool, format!("INSERT INTO schema_compat (version, min_binary) VALUES ({0}, {0})", known + 2)).await;
    assert!(PgStore::connect(db.url()).await.is_err(), "a migration that asks for a newer binary stops it");
    pool.close().await;
    db.cleanup().await;
}
