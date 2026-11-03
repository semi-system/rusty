//! PG2 spike measurements. `#[ignore]`d: they print numbers, they do not gate CI.
//!
//!   RUSTY_TEST_PG_URL=... cargo test -p rusty-store --features postgres --test spike_measure \
//!       -- --ignored --nocapture --test-threads=1
//!
//! The Postgres under test is the throw-away loopback container (tmpfs, fsync off): absolute
//! numbers say nothing about production, only relative behaviour (statement counts, contention
//! shape, batch vs single) is meaningful. SQLite is a file database in a temp dir (WAL, default
//! synchronous), pool of 4 on both.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::backends::{acct, backends, Backend, INBOX};
use common::{raw, stmts};
use rusty_core::Flags;
use rusty_store::Mailbox;

static TAG: AtomicUsize = AtomicUsize::new(0);

fn pct(v: &mut [Duration], p: f64) -> Duration {
    v.sort();
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn statements_per_operation() {
    let probe = stmts::probe();
    for b in backends().await {
        let n = b.name;
        let dom = b.dir.create_domain("m.test").await.unwrap();
        probe.reset();
        let acc = b.dir.create_account("alice", dom, None, None).await.unwrap();
        let create_account = probe.count();
        let mb = Mailbox::open_with(b.mrepo.clone(), b.blobs.clone(), acc).await.unwrap();
        probe.reset();
        mb.create_folder(None, "F").await.unwrap();
        let create_folder = probe.count();
        probe.reset();
        mb.append(INBOX, raw("one", Some("1@x")), Flags::default()).await.unwrap();
        let append = probe.count();
        probe.reset();
        mb.append(INBOX, raw("two", Some("2@x")), Flags::default()).await.unwrap();
        let append_hot = probe.count();
        probe.reset();
        mb.append_many(
            INBOX,
            (0..10).map(|i| raw(&format!("b{i}"), Some(&format!("b{i}@x")))).collect(),
            Flags::default(),
        )
        .await
        .unwrap();
        let many10 = probe.count();
        probe.reset();
        mb.append_many(
            INBOX,
            (0..100).map(|i| raw(&format!("c{i}"), Some(&format!("c{i}@x")))).collect(),
            Flags::default(),
        )
        .await
        .unwrap();
        let many100 = probe.count();
        probe.reset();
        mb.list_folder(INBOX, 0, 100).await.unwrap();
        let list = probe.count();
        probe.reset();
        mb.changes_since(0, 100).await.unwrap();
        let changes = probe.count();
        println!(
            "STMTS {n:9} create_account={create_account} create_folder={create_folder} \
             append={append} append(2nd)={append_hot} append_many(10)={many10} \
             append_many(100)={many100} list_folder={list} changes_since={changes}"
        );
    }
}

async fn run_workload(b: &Backend, label: &str, mailboxes: usize, conc: usize, per_task: usize, same_blob: bool) {
    let mut boxes: Vec<Mailbox> = vec![];
    for _ in 0..mailboxes {
        let (_, _, mb) = acct(b, &format!("w{}", TAG.fetch_add(1, Ordering::Relaxed))).await;
        boxes.push(mb);
    }
    let started = Instant::now();
    let mut tasks = vec![];
    for t in 0..conc {
        let mb = boxes[t % mailboxes].clone();
        tasks.push(tokio::spawn(async move {
            let mut lat = vec![];
            for i in 0..per_task {
                let body: Bytes = if same_blob {
                    raw("shared", Some("shared@x"))
                } else {
                    raw(&format!("t{t}-{i}"), Some(&format!("t{t}-{i}@x")))
                };
                let s = Instant::now();
                mb.append(INBOX, body, Flags::default()).await.unwrap();
                lat.push(s.elapsed());
            }
            lat
        }));
    }
    let mut lat = vec![];
    for t in tasks {
        lat.extend(t.await.unwrap());
    }
    let total = started.elapsed();
    let ops = lat.len();
    println!(
        "LOAD  {:9} {label:28} mailboxes={mailboxes:2} tasks={conc:2} ops={ops:4} \
         {:7.0} ops/s p50={:?} p99={:?}",
        b.name,
        ops as f64 / total.as_secs_f64(),
        pct(&mut lat, 0.5),
        pct(&mut lat, 0.99),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn concurrency_shapes() {
    for b in backends().await {
        run_workload(&b, "1 mailbox, 1 writer", 1, 1, 200, false).await;
        run_workload(&b, "1 mailbox, 4 writers", 1, 4, 50, false).await;
        run_workload(&b, "1 mailbox, 16 writers", 1, 16, 15, false).await;
        run_workload(&b, "8 mailboxes, 8 writers", 8, 8, 25, false).await;
        run_workload(&b, "8 mailboxes, same blob", 8, 8, 25, true).await;
        run_workload(&b, "1 mailbox, same blob, 8", 1, 8, 25, true).await;

        // Batch vs single for 200 messages into one mailbox.
        let (_, _, mb) = acct(&b, &format!("batch{}", b.name)).await;
        let s = Instant::now();
        for i in 0..200 {
            mb.append(INBOX, raw(&format!("s{i}"), None), Flags::default()).await.unwrap();
        }
        let single = s.elapsed();
        let s = Instant::now();
        for chunk in 0..2 {
            mb.append_many(
                INBOX,
                (0..100).map(|i| raw(&format!("m{chunk}-{i}"), None)).collect(),
                Flags::default(),
            )
            .await
            .unwrap();
        }
        let batched = s.elapsed();
        println!(
            "BATCH {:9} 200 single appends={single:?}  2 x append_many(100)={batched:?}",
            b.name
        );
    }
}

/// Latency of one append into a COLD mailbox while 12 tasks hammer ONE hot mailbox. On
/// Postgres every waiter of the hot mailbox's row lock holds a pooled connection, so with a
/// small pool the cold mailbox queues behind them in `acquire` (head-of-line blocking through
/// the pool); SQLite has one global write lock and shows its own, different queueing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn head_of_line_blocking_through_the_pool() {
    async fn cold_latency(hot: &Mailbox, cold: &Mailbox, hot_writers: usize) -> Vec<Duration> {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut tasks = vec![];
        for t in 0..hot_writers {
            let (hot, stop) = (hot.clone(), stop.clone());
            tasks.push(tokio::spawn(async move {
                let mut i = 0;
                while !stop.load(Ordering::Relaxed) {
                    hot.append(INBOX, raw(&format!("h{t}-{i}"), None), Flags::default())
                        .await
                        .unwrap();
                    i += 1;
                }
            }));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut lat = vec![];
        for i in 0..40 {
            let s = Instant::now();
            cold.append(INBOX, raw(&format!("c{i}"), None), Flags::default()).await.unwrap();
            lat.push(s.elapsed());
        }
        stop.store(true, Ordering::Relaxed);
        for t in tasks {
            t.await.unwrap();
        }
        lat
    }

    for b in backends().await {
        let (_, hot_acc, hot) = acct(&b, &format!("hot{}", TAG.fetch_add(1, Ordering::Relaxed))).await;
        let (_, cold_acc, cold) = acct(&b, &format!("cold{}", TAG.fetch_add(1, Ordering::Relaxed))).await;
        let _ = (hot_acc, cold_acc);
        for writers in [0usize, 12] {
            let mut lat = cold_latency(&hot, &cold, writers).await;
            println!(
                "HOL   {:9} pool=4 hot_writers={writers:2}: cold-mailbox append p50={:?} p99={:?}",
                b.name,
                pct(&mut lat, 0.5),
                pct(&mut lat, 0.99)
            );
        }
        #[cfg(feature = "postgres")]
        if let Some(db) = &b._db {
            use rusty_store::repo::pg::PgRepo;
            let cfg = rusty_store::PgPoolConfig {
                max_connections: 16,
                ..Default::default()
            };
            let store = rusty_store::PgStore::connect_with_config(db.url(), cfg).await.unwrap();
            let repo = std::sync::Arc::new(PgRepo::new(store));
            let hot16 = Mailbox::open_with(repo.clone(), b.blobs.clone(), hot_acc).await.unwrap();
            let cold16 = Mailbox::open_with(repo, b.blobs.clone(), cold_acc).await.unwrap();
            let mut lat = cold_latency(&hot16, &cold16, 12).await;
            println!(
                "HOL   {:9} pool=16 hot_writers=12: cold-mailbox append p50={:?} p99={:?}",
                b.name,
                pct(&mut lat, 0.5),
                pct(&mut lat, 0.99)
            );
        }
    }
}
