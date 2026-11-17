//! `DirectoryRepo::record_login` (migration 0029): the time of an account's last sign-in is written once an hour at most, however often it signs in; an account that never signed in has none;
//! and the services that check a password may write that one column of an account and nothing else of it.

mod common;

use common::pg::{pg_admin_url, PgTestDb};
use rusty_store::repo::pg::PgRepo;
use rusty_store::repo::{AccountInfo, DirectoryRepo, NewAccount};
use sqlx::postgres::PgPoolOptions;

async fn account(repo: &PgRepo, local: &str, domain: rusty_core::DomainId) -> AccountInfo {
    let id = repo.create_account(&NewAccount { local: local.into(), domain, plan: None, password_hash: None }).await.unwrap();
    repo.find_account(local, "one.test").await.unwrap().unwrap_or_else(|| panic!("{local} ({id:?}) was just made"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sign_in_is_recorded_at_most_once_an_hour_and_an_account_that_never_signed_in_has_no_time() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let store = db.connect().await;
    let repo = PgRepo::new(store.clone());
    let domain = repo.create_domain("one.test").await.unwrap();
    let alice = account(&repo, "alice", domain).await;
    assert_eq!(alice.last_login_at, None, "never signed in");

    repo.record_login(alice.id).await.unwrap();
    let first = repo.find_account("alice", "one.test").await.unwrap().unwrap().last_login_at.expect("the first sign-in is recorded");
    let now: i64 = sqlx::query_scalar("SELECT rs_now()").fetch_one(store.pool()).await.unwrap();
    assert!((now - first).abs() <= 5, "{first} against {now}");

    // Within the hour nothing is written: the stored time stays what it was, even when it is old by a few minutes.
    sqlx::query("UPDATE accounts SET last_login_at = rs_now() - 600 WHERE id = $1").bind(alice.id.0).execute(store.pool()).await.unwrap();
    let ten_minutes_ago: i64 = sqlx::query_scalar("SELECT last_login_at FROM accounts WHERE id = $1").bind(alice.id.0).fetch_one(store.pool()).await.unwrap();
    repo.record_login(alice.id).await.unwrap();
    assert_eq!(repo.find_account("alice", "one.test").await.unwrap().unwrap().last_login_at, Some(ten_minutes_ago), "a sign-in ten minutes after the last one writes nothing");

    // Past the hour it is written again.
    sqlx::query("UPDATE accounts SET last_login_at = rs_now() - 7200 WHERE id = $1").bind(alice.id.0).execute(store.pool()).await.unwrap();
    repo.record_login(alice.id).await.unwrap();
    let again = repo.find_account("alice", "one.test").await.unwrap().unwrap().last_login_at.unwrap();
    assert!(again > ten_minutes_ago, "{again} against {ten_minutes_ago}");

    // An unknown account is not an error, and it makes no row.
    repo.record_login(rusty_core::AccountId(9_999_999)).await.unwrap();
    // The list and the search carry the time too.
    let page = repo.list_accounts(domain, None, 10).await.unwrap();
    assert_eq!(page[0].last_login_at, Some(again));
    db.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_services_that_check_a_password_write_the_time_of_a_sign_in_and_nothing_else_of_an_account() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&admin).await;
    let admin_pool = PgPoolOptions::new().max_connections(2).connect(db.admin_db_url()).await.unwrap();
    let domain: i64 = sqlx::query_scalar("INSERT INTO domains (name, created_at) VALUES ('one.test', 1) RETURNING id").fetch_one(&admin_pool).await.unwrap();
    let account: i64 = sqlx::query_scalar("INSERT INTO accounts (local_part, domain_id, status, created_at) VALUES ('alice', $1, 'active', 1) RETURNING id").bind(domain).fetch_one(&admin_pool).await.unwrap();

    for role in ["rusty_imap", "rusty_submission"] {
        let pool = PgPoolOptions::new().max_connections(2).connect(&db.role_url(role)).await.unwrap();
        // The statement the repository runs, as the role.
        let done = sqlx::query("UPDATE accounts SET last_login_at = rs_now() WHERE id = $1 AND (last_login_at IS NULL OR last_login_at < rs_now() - 3600)").bind(account).execute(&pool).await;
        assert!(done.is_ok(), "{role} must record a sign-in: {done:?}");
        sqlx::query("UPDATE accounts SET last_login_at = NULL WHERE id = $1").bind(account).execute(&admin_pool).await.unwrap();
        for forbidden in ["UPDATE accounts SET status = 'disabled'", "UPDATE accounts SET password_hash = 'x'", "UPDATE accounts SET local_part = 'x'", "DELETE FROM accounts"] {
            let denied = sqlx::query(sqlx::AssertSqlSafe(forbidden)).execute(&pool).await;
            let code = denied.as_ref().err().and_then(|e| e.as_database_error()).and_then(|d| d.code().map(|c| c.into_owned()));
            assert_eq!(code.as_deref(), Some("42501"), "{role}: {forbidden}");
        }
    }
    for role in ["rusty_smtp_in", "rusty_outbound", "rusty_gc"] {
        let pool = PgPoolOptions::new().max_connections(2).connect(&db.role_url(role)).await.unwrap();
        let denied = sqlx::query("UPDATE accounts SET last_login_at = rs_now() WHERE id = $1").bind(account).execute(&pool).await;
        let code = denied.as_ref().err().and_then(|e| e.as_database_error()).and_then(|d| d.code().map(|c| c.into_owned()));
        assert_eq!(code.as_deref(), Some("42501"), "{role} does not check a password and records no sign-in");
    }
    db.cleanup().await;
}
