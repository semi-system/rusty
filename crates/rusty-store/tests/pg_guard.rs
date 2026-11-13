//! The Postgres test guard must leave no database behind, also when the test panics.

mod common;

use std::process::Command;

use common::pg::{parse_require_pg, pg_admin_url, PgTestDb};
use sqlx::postgres::PgPoolOptions;

const CHILD_ENV: &str = "RUSTY_GUARD_CHILD_NAME_FILE";

/// Runs only as the child of `panicking_test_leaves_no_database`: creates a database and panics
/// while the guard is alive.
#[tokio::test]
async fn guard_child_panics_on_purpose() {
    let Ok(file) = std::env::var(CHILD_ENV) else {
        return; // not the child: nothing to do
    };
    let admin = pg_admin_url().expect("child needs RUSTY_TEST_PG_URL");
    let db = PgTestDb::create(&admin).await;
    std::fs::write(&file, db.name()).unwrap();
    let _guard = db;
    panic!("deliberate panic while the test database guard is alive");
}

#[tokio::test]
async fn panicking_test_leaves_no_database() {
    let Some(admin_url) = pg_admin_url() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let name_file = dir.path().join("dbname");
    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .args(["guard_child_panics_on_purpose", "--exact", "--test-threads=1"])
        .env(CHILD_ENV, &name_file)
        .output()
        .unwrap();
    assert!(!out.status.success(), "the child must fail (it panics on purpose)");
    let name = std::fs::read_to_string(&name_file).expect("the child created a database");
    assert!(name.starts_with("rusty_test_"), "{name}");

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_database WHERE datname = $1")
        .bind(&name)
        .fetch_one(&admin)
        .await
        .unwrap();
    assert_eq!(left, 0, "the guard left database {name} behind");
}

#[test]
fn require_pg_accepts_only_one_zero_or_unset() {
    assert!(!parse_require_pg(None));
    assert!(!parse_require_pg(Some("")));
    assert!(!parse_require_pg(Some("0")));
    assert!(parse_require_pg(Some("1")));
    for bad in ["true", "yes", "2", "01", " 1"] {
        assert!(
            std::panic::catch_unwind(|| parse_require_pg(Some(bad))).is_err(),
            "{bad:?} must be rejected"
        );
    }
}

#[test]
fn bad_require_pg_is_rejected_even_when_the_url_is_set() {
    use common::pg::admin_url_from;
    let url = Some("postgres://u:p@127.0.0.1/db".to_string());
    for bad in ["true", "yes", "2", "01", " 1"] {
        let u = url.clone();
        assert!(
            std::panic::catch_unwind(|| admin_url_from(u, Some(bad))).is_err(),
            "RUSTY_REQUIRE_PG={bad:?} must be rejected although the URL is set"
        );
    }
    assert!(admin_url_from(url.clone(), Some("1")).is_some());
    assert!(admin_url_from(url, None).is_some());
    // Postgres is mandatory: a missing or empty URL fails the test, whatever RUSTY_REQUIRE_PG says.
    for (u, require) in [(None, None), (None, Some("0")), (None, Some("1")), (Some(String::new()), Some("0"))] {
        assert!(
            std::panic::catch_unwind(|| admin_url_from(u.clone(), require)).is_err(),
            "{u:?} / {require:?} must fail, not skip"
        );
    }
}
