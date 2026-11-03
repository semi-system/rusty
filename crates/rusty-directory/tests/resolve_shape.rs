//! `resolve` costs at most two statements. The counter is test-only: sqlx emits one
//! `sqlx::query` tracing event per executed statement. It is process-global (sqlx runs
//! statements on a worker thread), so this file holds a single test.

use std::sync::atomic::{AtomicUsize, Ordering};

use rusty_directory::{Directory, Resolved};
use rusty_store::SqliteStore;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

static COUNT: AtomicUsize = AtomicUsize::new(0);

struct Counter;

impl Subscriber for Counter {
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        m.target() == "sqlx::query"
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, e: &Event<'_>) {
        if e.metadata().target() == "sqlx::query" {
            COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

async fn cost(d: &Directory, addr: &str) -> (usize, rusty_core::Result<Resolved>) {
    COUNT.store(0, Ordering::SeqCst);
    let r = d.resolve(addr).await;
    (COUNT.load(Ordering::SeqCst), r)
}

#[tokio::test]
async fn resolve_uses_at_most_two_queries() {
    tracing::subscriber::set_global_default(Counter).unwrap();
    let d = Directory::new(SqliteStore::open("sqlite::memory:").await.unwrap());
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("alice", dom, None, None).await.unwrap();
    d.add_alias("ally", dom, acc).await.unwrap();
    let list = d.create_list("team", dom).await.unwrap();
    for i in 0..50 {
        d.add_list_member(list, None, Some(&format!("u{i}@other.org")))
            .await
            .unwrap();
    }
    let (n, r) = cost(&d, "alice@example.com").await;
    assert!(matches!(r, Ok(Resolved::Account(_))));
    assert!((1..=2).contains(&n), "account: {n} statements");
    let (n, r) = cost(&d, "ally@example.com").await;
    assert!(matches!(r, Ok(Resolved::Account(_))));
    assert!((1..=2).contains(&n), "alias: {n} statements");
    let (n, r) = cost(&d, "team@example.com").await;
    assert!(matches!(r, Ok(Resolved::List(ref m)) if m.len() == 50));
    assert!((1..=2).contains(&n), "list: {n} statements");
    let (n, r) = cost(&d, "nobody@example.com").await;
    assert!(r.is_err());
    assert!((1..=2).contains(&n), "unknown: {n} statements");
    let (n, r) = cost(&d, "a@nowhere.com").await;
    assert!(r.is_err());
    assert!((1..=2).contains(&n), "unknown domain: {n} statements");
}
