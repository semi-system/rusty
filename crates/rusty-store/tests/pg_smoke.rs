#![cfg(feature = "postgres")]

mod common;

use std::time::Duration;

use common::pg::{pg_admin_url, PgTestDb, PG_SKIP_NOTICE};
use rusty_core::Error;
use rusty_store::{PgPoolConfig, PgStore};
use url::Url;

#[test]
fn pool_defaults_are_stable() {
    let config = PgPoolConfig::default();
    assert_eq!(config.max_connections, 4);
    assert_eq!(config.acquire_timeout, Duration::from_secs(5));
    assert_eq!(config.statement_timeout, Duration::from_secs(30));
    assert_eq!(config.idle_in_transaction_timeout, Duration::from_secs(30));
    assert_eq!(config.idle_timeout, Duration::from_secs(5 * 60));
    assert_eq!(config.max_lifetime, Duration::from_secs(30 * 60));
}

#[tokio::test]
async fn pg_reports_version_18() {
    let Some(admin_url) = pg_admin_url() else {
        return;
    };
    let db = PgTestDb::create(&admin_url).await;
    let store = db.connect().await;
    let version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::integer")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!((180_000..190_000).contains(&version), "version={version}");
    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn isolated_databases_do_not_see_each_other() {
    let Some(admin_url) = pg_admin_url() else {
        return;
    };
    let a = PgTestDb::create(&admin_url).await;
    let b = PgTestDb::create(&admin_url).await;
    let sa = a.connect().await;
    let sb = b.connect().await;
    sqlx::query("CREATE TABLE only_in_a (id bigint PRIMARY KEY)")
        .execute(sa.pool())
        .await
        .unwrap();
    let seen: Option<String> = sqlx::query_scalar("SELECT to_regclass('public.only_in_a')::text")
        .fetch_one(sb.pool())
        .await
        .unwrap();
    assert_eq!(seen, None);
    sa.close().await;
    sb.close().await;
    a.cleanup().await;
    b.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_is_bounded_and_session_timeouts_are_applied() {
    let Some(admin_url) = pg_admin_url() else {
        return;
    };
    let db = PgTestDb::create(&admin_url).await;
    let config = PgPoolConfig {
        max_connections: 2,
        acquire_timeout: Duration::from_secs(2),
        statement_timeout: Duration::from_millis(100),
        idle_in_transaction_timeout: Duration::from_millis(150),
        ..PgPoolConfig::default()
    };
    let store = PgStore::connect_with_config(db.url(), config)
        .await
        .unwrap();

    let statement: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let idle: String = sqlx::query_scalar("SHOW idle_in_transaction_session_timeout")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(statement, "100ms");
    assert_eq!(idle, "150ms");

    let mut tx = store.begin_write().await.unwrap();
    let isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(isolation, "read committed");
    tx.rollback().await.unwrap();

    let mut held = Vec::new();
    for _ in 0..2 {
        held.push(store.pool().acquire().await.unwrap());
    }
    assert_eq!(store.pool().size(), 2);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), store.pool().acquire())
            .await
            .is_err()
    );
    drop(held);

    let timed_out = sqlx::query("SELECT pg_sleep(1)")
        .execute(store.pool())
        .await
        .unwrap_err();
    assert_eq!(
        timed_out
            .as_database_error()
            .and_then(|e| e.code())
            .as_deref(),
        Some("57014")
    );

    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn wrong_password_is_sanitized() {
    let Some(admin_url) = pg_admin_url() else {
        return;
    };
    let secret = "pg0-do-not-leak-this-password";
    let mut url = Url::parse(&admin_url).unwrap();
    url.set_password(Some(secret)).unwrap();
    let rendered_url: String = url.into();
    let err = PgStore::connect(&rendered_url).await.unwrap_err();
    assert!(matches!(err, Error::Storage { .. }));
    let display = err.to_string();
    let debug = format!("{err:?}");
    for rendered in [&display, &debug] {
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains(&rendered_url));
        assert!(!rendered.contains("127.0.0.1"));
    }
}

#[test]
fn skip_notice_is_stable_and_contains_no_secret() {
    assert_eq!(
        PG_SKIP_NOTICE,
        "POSTGRES_TESTS_SKIPPED: RUSTY_TEST_PG_URL is not set; no Postgres test server was used"
    );
    assert!(!PG_SKIP_NOTICE.contains("postgres://"));
}

#[tokio::test]
async fn database_is_dropped_even_when_the_test_panics() {
    let Some(admin_url) = pg_admin_url() else {
        return;
    };
    let url = admin_url.clone();
    let worker = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = PgTestDb::create(&url).await;
            let _ = NAME.set(db.name().to_owned());
            panic!("simulated test failure");
        });
    });
    assert!(worker.join().is_err(), "the worker must have panicked");
    let name = NAME.get().expect("name recorded").clone();
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_database WHERE datname = $1")
        .bind(&name)
        .fetch_one(&admin)
        .await
        .unwrap();
    assert_eq!(left, 0, "the panicking test leaked its database");
}

static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
