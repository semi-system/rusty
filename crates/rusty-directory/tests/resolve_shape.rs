//! `resolve` costs at most two statements. The counter (`rusty_store::fixtures::StatementProbe`)
//! is process-global (the driver runs statements on a worker thread), so this file holds a single
//! test, which runs both backends one after the other.

mod common;

use common::for_each_backend;
use rusty_directory::{Directory, Resolved};
use rusty_store::fixtures::StatementProbe;

async fn cost(p: &StatementProbe, d: &Directory, addr: &str) -> (usize, rusty_core::Result<Resolved>) {
    p.reset();
    let r = d.resolve(addr).await;
    (p.count(), r)
}

#[tokio::test]
async fn resolve_uses_at_most_two_queries() {
    let p = StatementProbe::start();
    for_each_backend(|b| {
        let p = &p;
        async move {
            let d = b.dir.clone();
            let dom = d.create_domain("example.com").await.unwrap();
            let acc = d.create_account("alice", dom, None, None).await.unwrap();
            d.add_alias("ally", dom, acc).await.unwrap();
            let list = d.create_list("team", dom).await.unwrap();
            for i in 0..50 {
                d.add_list_member(list, None, Some(&format!("u{i}@other.org")))
                    .await
                    .unwrap();
            }
            let (n, r) = cost(p, &d, "alice@example.com").await;
            assert!(matches!(r, Ok(Resolved::Account(_))));
            assert!((1..=2).contains(&n), "{}: account: {n} statements", b.name);
            let (n, r) = cost(p, &d, "ally@example.com").await;
            assert!(matches!(r, Ok(Resolved::Account(_))));
            assert!((1..=2).contains(&n), "{}: alias: {n} statements", b.name);
            let (n, r) = cost(p, &d, "team@example.com").await;
            assert!(matches!(r, Ok(Resolved::List(ref m)) if m.len() == 50));
            assert!((1..=2).contains(&n), "{}: list: {n} statements", b.name);
            let (n, r) = cost(p, &d, "nobody@example.com").await;
            assert!(r.is_err());
            assert!((1..=2).contains(&n), "{}: unknown: {n} statements", b.name);
            let (n, r) = cost(p, &d, "a@nowhere.com").await;
            assert!(r.is_err());
            assert!((1..=2).contains(&n), "{}: unknown domain: {n} statements", b.name);
        }
    })
    .await;
}
