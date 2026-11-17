//! The announcements of the admin console's live updates (migration 0028): each table that a page shows announces what changed, once per transaction and domain, at COMMIT only; the
//! roles of the services (here the outbound service recording a check) can write what they write with the triggers in place; and the feed delivers them to a sink, with a `resync` when it
//! subscribes.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::pg::{pg_admin_url, PgTestDb};
use rusty_store::{AdminChange, AdminKind, AdminSink};
use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl Recorder {
    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn count(&self, what: &str) -> usize {
        self.all().iter().filter(|e| *e == what).count()
    }
    async fn until(&self, what: &str, pred: impl Fn(&[String]) -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if pred(&self.all()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}; saw {:?}", self.all());
    }
}

impl AdminSink for Recorder {
    fn changed(&self, c: AdminChange) {
        let kind = match c.kind {
            AdminKind::Domains => "domains",
            AdminKind::Accounts => "accounts",
            AdminKind::Dns => "dns",
            AdminKind::Outbound => "outbound",
            AdminKind::Storage => "storage",
            AdminKind::Admins => "admins",
        };
        self.0.lock().unwrap().push(format!("{kind}:{}", c.domain.map(|d| d.to_string()).unwrap_or_default()));
    }
    fn resync(&self) {
        self.0.lock().unwrap().push("resync".into());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_table_a_page_shows_announces_its_change_once_per_transaction_and_domain_and_only_at_commit() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&admin).await;
    let store = db.connect().await;
    let pool = PgPoolOptions::new().max_connections(2).connect(db.admin_db_url()).await.unwrap();
    let outbound = PgPoolOptions::new().max_connections(2).connect(&db.role_url("rusty_outbound")).await.unwrap();

    let rec = Arc::new(Recorder::default());
    let stop = CancellationToken::new();
    let feed = store.admin_feed();
    let task = tokio::spawn({
        let (rec, stop) = (rec.clone(), stop.clone());
        async move { feed.run(rec, stop).await }
    });
    rec.until("the first resync", |e| e.iter().any(|x| x == "resync")).await;

    let domain: i64 = sqlx::query_scalar("INSERT INTO domains (name, created_at) VALUES ('one.test', 1) RETURNING id").fetch_one(&pool).await.unwrap();
    rec.until("domains", |e| e.iter().any(|x| x == "domains:")).await;

    let account: i64 = sqlx::query_scalar("INSERT INTO accounts (local_part, domain_id, status, created_at) VALUES ('alice', $1, 'active', 1) RETURNING id").bind(domain).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO aliases (local_part, domain_id, account_id) VALUES ('postmaster', $1, $2)").bind(domain).bind(account).execute(&pool).await.unwrap();
    rec.until("accounts", |e| e.iter().any(|x| x == &format!("accounts:{domain}"))).await;

    sqlx::query("INSERT INTO admin_grants (account_id, domain_id, created_at) VALUES ($1, $2, 1)").bind(account).bind(domain).execute(&pool).await.unwrap();
    rec.until("admins", |e| e.iter().any(|x| x == "admins:")).await;

    sqlx::query("INSERT INTO domain_relays (domain_id, host, port, tls, created_at, updated_at) VALUES ($1, 'smtp.relay.example', 587, 'starttls', 1, 1)").bind(domain).execute(&pool).await.unwrap();
    rec.until("outbound", |e| e.iter().any(|x| x == &format!("outbound:{domain}"))).await;

    // A store of the domain and a backend of that store; what the outbound service records afterwards announces the domain, with the privileges that role really has.
    let store_id: i64 = sqlx::query_scalar("INSERT INTO blob_stores (domain_id, kind, created_at) VALUES ($1, 's3', 1) RETURNING id").bind(domain).fetch_one(&pool).await.unwrap();
    rec.until("storage", |e| e.iter().any(|x| x == &format!("storage:{domain}"))).await;
    let before = rec.count(&format!("storage:{domain}"));
    let done = sqlx::query("UPDATE blob_stores SET check_status = 'ok', checked_at = 5 WHERE id = $1").bind(store_id).execute(&outbound).await.unwrap();
    assert_eq!(done.rows_affected(), 1);
    rec.until("the check the outbound role recorded", |e| e.iter().filter(|x| **x == format!("storage:{domain}")).count() > before).await;
    let backend: i64 = sqlx::query_scalar("INSERT INTO blob_backends (store_id, name, kind, created_at) VALUES ($1, 'b1', 's3', 1) RETURNING id").bind(store_id).fetch_one(&pool).await.unwrap();
    let before = rec.count(&format!("storage:{domain}"));
    sqlx::query("UPDATE blob_backends SET check_status = 'ok', checked_at = 5 WHERE id = $1").bind(backend).execute(&outbound).await.unwrap();
    rec.until("the check of a backend, announced for its store's domain", |e| e.iter().filter(|x| **x == format!("storage:{domain}")).count() > before).await;

    // The DNS state the outbound service writes.
    sqlx::query(
        "INSERT INTO domain_dns_state (domain_id, record_name, record_type, purpose, expected_value_hash, last_status, last_checked_at, status_since) \
         VALUES ($1, 'one.test.', 'MX', 'mail', repeat('a', 64), 'ok', 1, 1)",
    )
    .bind(domain)
    .execute(&outbound)
    .await
    .unwrap();
    rec.until("dns", |e| e.iter().any(|x| x == &format!("dns:{domain}"))).await;

    // Once per transaction and domain, however many rows: two hundred records of one domain are one announcement.
    let before = rec.count(&format!("dns:{domain}"));
    let mut tx = pool.begin().await.unwrap();
    for i in 0..200 {
        sqlx::query("UPDATE domain_dns_state SET last_status = $2 WHERE domain_id = $1 AND record_name = 'one.test.'").bind(domain).bind(if i % 2 == 0 { "missing" } else { "ok" }).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    rec.until("one announcement for the transaction", |e| e.iter().filter(|x| **x == format!("dns:{domain}")).count() > before).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rec.count(&format!("dns:{domain}")), before + 1, "{:?}", rec.all());

    // A rolled-back change is never announced.
    let before = rec.all().len();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE domain_relays SET port = 465, tls = 'implicit' WHERE domain_id = $1").bind(domain).execute(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(rec.all().len(), before, "a rollback was announced: {:?}", rec.all());

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("the feed must stop").unwrap().unwrap();
    db.cleanup().await;
}
