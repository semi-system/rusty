//! The heartbeats of the services (migration 0027, docs/design-admin-monitor.md): renewal, one row per instance, the age by the database clock, the cleanup, and the rule that a
//! service role can only write its own service's row.

mod common;

use common::pg::{pg_admin_url, PgTestDb};
use rusty_store::repo::pg::PgRepo;
use rusty_store::{HeartbeatRepo, PgStore};
use sqlx::postgres::PgPoolOptions;

async fn repo(url: &str) -> PgRepo {
    PgRepo::new(PgStore::connect(url).await.unwrap())
}

#[tokio::test]
async fn a_beat_renews_its_row_and_each_instance_has_its_own() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create(&url).await;
    let r = repo(db.url()).await;
    r.beat("imap", "host-a", "1.0.0", 100, "ok", "{\"listeners\":2}").await.unwrap();
    r.beat("imap", "host-b", "1.0.0", 5, "ok", "{}").await.unwrap();
    r.beat("gc", "host-c", "1.0.0", 7, "degraded", "{}").await.unwrap();
    let rows = r.list().await.unwrap();
    assert_eq!(rows.iter().map(|x| (x.service.as_str(), x.instance.as_str())).collect::<Vec<_>>(), vec![("gc", "host-c"), ("imap", "host-a"), ("imap", "host-b")]);
    let a = rows.iter().find(|x| x.instance == "host-a").unwrap();
    assert!((100..=103).contains(&a.uptime_secs), "the uptime counts from the start the service reported: {}", a.uptime_secs);
    assert!((0..=2).contains(&a.age_secs), "the age is the database clock's: {}", a.age_secs);
    assert_eq!((a.version.as_str(), a.status.as_str(), a.detail.as_str()), ("1.0.0", "ok", "{\"listeners\":2}"));
    // The same instance again is the same row, with the new facts.
    r.beat("imap", "host-a", "1.0.1", 120, "degraded", "{\"listeners\":1}").await.unwrap();
    let rows = r.list().await.unwrap();
    assert_eq!(rows.len(), 3);
    let a = rows.iter().find(|x| x.instance == "host-a").unwrap();
    assert_eq!((a.version.as_str(), a.status.as_str()), ("1.0.1", "degraded"));
}

#[tokio::test]
async fn a_beat_that_breaks_the_table_rules_is_refused() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create(&url).await;
    let r = repo(db.url()).await;
    assert!(r.beat("mailman", "h", "1", 1, "ok", "{}").await.is_err(), "not a service");
    assert!(r.beat("imap", "", "1", 1, "ok", "{}").await.is_err(), "no instance name");
    assert!(r.beat("imap", "h", "1", 1, "fine", "{}").await.is_err(), "not a status");
    assert!(r.beat("imap", "h", "1", 1, "ok", &"x".repeat(3000)).await.is_err(), "the detail is bounded");
    assert!(r.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_cleanup_removes_only_the_rows_nobody_renews() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create(&url).await;
    let r = repo(db.url()).await;
    r.beat("imap", "fresh", "1", 1, "ok", "{}").await.unwrap();
    r.beat("imap", "old", "1", 1, "ok", "{}").await.unwrap();
    let admin = PgPoolOptions::new().max_connections(1).connect(db.admin_db_url()).await.unwrap();
    sqlx::query("UPDATE service_heartbeats SET last_seen = last_seen - 100000 WHERE instance = 'old'").execute(&admin).await.unwrap();
    assert_eq!(r.prune(86_400).await.unwrap(), 1);
    let rows = r.list().await.unwrap();
    assert_eq!(rows.iter().map(|x| x.instance.as_str()).collect::<Vec<_>>(), vec!["fresh"]);
    assert_eq!(r.prune(86_400).await.unwrap(), 0, "nothing else is old");
}

#[tokio::test]
async fn a_service_role_writes_only_its_own_service_whatever_it_passes() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let imap = repo(&db.role_url("rusty_imap")).await;
    imap.beat("smtp-in", "forged", "1", 1, "ok", "{}").await.unwrap();
    let outbound = repo(&db.role_url("rusty_outbound")).await;
    outbound.beat("gc", "forged-too", "1", 1, "ok", "{}").await.unwrap();
    let admin_role = repo(&db.role_url("rusty_admin")).await;
    let rows = admin_role.list().await.unwrap();
    let seen: Vec<(&str, &str)> = rows.iter().map(|x| (x.service.as_str(), x.instance.as_str())).collect();
    assert_eq!(seen, vec![("imap", "forged"), ("outbound", "forged-too")], "the role decides the service, not the caller");
    // A service role has no write of its own on the table: only the function.
    let pool = PgPoolOptions::new().max_connections(1).connect(&db.role_url("rusty_imap")).await.unwrap();
    for sql in ["INSERT INTO service_heartbeats (service, instance, version, started_at, last_seen, status) VALUES ('gc', 'x', '1', 0, 0, 'ok')", "UPDATE service_heartbeats SET status = 'ok'", "DELETE FROM service_heartbeats"] {
        let e = sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&pool).await.unwrap_err();
        assert_eq!(e.as_database_error().and_then(|d| d.code().map(|c| c.into_owned())).as_deref(), Some("42501"), "{sql}");
    }
    // Only the collector deletes, through the cleanup; the admin web only reads.
    assert!(admin_role.prune(0).await.is_err(), "the admin web does not clean up");
}
