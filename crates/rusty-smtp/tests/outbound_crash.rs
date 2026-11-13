//! `kill -9` of a REAL worker process in the middle of a delivery (plan U5): the test re-executes its own
//! binary as the worker (`child_worker`, selected by an environment variable), lets it hand the whole
//! message to a fake MTA that then never answers, kills the process with SIGKILL, and starts a second
//! worker on the same database and blob directory. Asserts: the message is not lost, the lease of the dead
//! worker lapses and the job is delivered by the new one, the sender gets no bounce, and the job ends
//! `done`. At-least-once is the documented outcome: the fake MTA has the message twice (the first copy came
//! from the process that died after sending and before the final reply).
//!
//! One Postgres database is shared by both processes (the child gets its URL in the environment); the
//! fencing behaviour with in-process workers is `outbound_worker::a_worker_that_dies_loses_its_lease_...`.
#![cfg(unix)]

mod common;

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::fake_mta::*;
use rusty_core::config::{OutboundConfig, StartupConfig};
use rusty_directory::Directory;
use rusty_smtp::outbound::route::ConfigRoute;
use rusty_smtp::outbound::tls::RustlsUpgrader;
use rusty_smtp::outbound::worker::{OutboundWorker, WorkerDeps, WorkerOptions};
use rusty_smtp::outbound_queue::JobOutbound;
use rusty_smtp::resolve::{MxAnswer, MxHost, StaticDns};
use rusty_store::fixtures::pg_db::{pg_admin_url, PgTestDb};
use rusty_store::fixtures::Fixtures;
use rusty_store::{BlobStore, FsBlobStore, Store, StoreConfig};
use rusty_svc::{Context, FileSecretSource, OutboundEnqueue, OutboundMessage, Service, TlsProvider};
use tokio_util::sync::CancellationToken;

const MSG: &str = "From: alice@example.net\r\nTo: bob@remote.test\r\nSubject: crash\r\nMessage-ID: <c@example.net>\r\n\r\nbody\r\n";

fn cfg() -> OutboundConfig {
    OutboundConfig { backoff_secs: vec![1], lease_secs: 3, connect_timeout_secs: 2, command_timeout_secs: 2, data_timeout_secs: 60, ..OutboundConfig::default() }
}

async fn context(dir: &std::path::Path, url: &str) -> (Context, Store) {
    let store = Store::open(StoreConfig::postgres(url)).await.unwrap();
    let blobs_root = dir.join("blobs");
    if !blobs_root.exists() {
        FsBlobStore::init(&blobs_root).await.unwrap();
    }
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::open(&blobs_root).await.unwrap());
    let mut sc = StartupConfig::dev(dir);
    sc.hostname = Some("mx.example.net".into());
    let ctx = Context::new(store.repos(), blobs, Arc::new(sc), TlsProvider::unavailable().unwrap()).await.unwrap();
    (ctx, store)
}

fn worker(mta: std::net::SocketAddr) -> OutboundWorker {
    let mut dns = StaticDns::default();
    dns.mx.insert("remote.test".into(), MxAnswer::Hosts(vec![MxHost { preference: 10, host: "mx.fake.test".into() }]));
    dns.addrs.insert("mx.fake.test".into(), vec!["192.0.2.10".parse().unwrap()]);
    let c = cfg();
    let deps = WorkerDeps {
        resolver: Arc::new(dns),
        dialer: MapDialer::new(&[("192.0.2.10", mta)]),
        tls: Arc::new(RustlsUpgrader::new(None).unwrap()),
        route: Arc::new(ConfigRoute::from_config(&c, &FileSecretSource::new()).unwrap()),
        signer: None,
    };
    let mut opts = WorkerOptions::new(c, "mx.example.net");
    opts.poll = Duration::from_millis(50);
    OutboundWorker::new(deps, opts)
}

/// The worker process: runs until it is killed. Only does anything when the parent set the variables.
#[tokio::test]
async fn child_worker() {
    let (Ok(dir), Ok(addr), Ok(url)) =
        (std::env::var("RUSTY_CRASH_DIR"), std::env::var("RUSTY_CRASH_MTA"), std::env::var("RUSTY_CRASH_URL"))
    else {
        return;
    };
    let (ctx, _store) = context(std::path::Path::new(&dir), &url).await;
    let _ = Box::new(worker(addr.parse().unwrap())).run(ctx, CancellationToken::new()).await;
}

#[tokio::test]
async fn a_worker_process_killed_mid_delivery_loses_nothing_and_the_job_is_delivered_by_the_next_one() {
    let dir = tempfile::tempdir().unwrap();
    // The first connection takes the message and never answers: the worker is "in the middle of DATA".
    let mta = FakeMta::start(|n| {
        let mut s = Script::default();
        if n == 1 {
            s.data_end = Box::new(|_| DataEnd::Hang);
        }
        s
    })
    .await;
    let db = PgTestDb::create(&pg_admin_url().expect("a Postgres test server (RUSTY_TEST_PG_URL)")).await;
    let (ctx, store) = context(dir.path(), db.url()).await;
    let fx = Fixtures::admin_of(&db, &store).await;
    let d = Directory::with_repo(store.repos().directory.clone());
    let dom = d.create_domain("example.net").await.unwrap();
    let alice = d.create_account("alice", dom, Some("pw-alice-1234"), None).await.unwrap();
    let blob = ctx.blobs.put(Bytes::from(MSG.as_bytes().to_vec())).await.unwrap();
    JobOutbound::new(ctx.jobs.clone(), ctx.directory.clone())
        .enqueue(OutboundMessage {
            blob,
            envelope_from: "alice@example.net".into(),
            recipients: vec!["bob@remote.test".into()],
            sender_domain: "example.net".into(),
            account: alice.0,
            submission_key: "k".into(),
            bytes: MSG.len() as u64,
        })
        .await
        .unwrap();

    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .args(["--exact", "child_worker", "--nocapture", "--test-threads", "1"])
        .env("RUSTY_CRASH_DIR", dir.path())
        .env("RUSTY_CRASH_MTA", mta.addr.to_string())
        .env("RUSTY_CRASH_URL", db.url())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if mta.messages().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(mta.messages().len(), 1, "the child worker delivered the whole message to the fake MTA");
    child.kill().unwrap(); // SIGKILL: no handler, no cleanup, no chance to settle the job
    child.wait().unwrap();
    let running = ctx.jobs.claim("probe", &["outbound.send"], 60).await.unwrap();
    assert!(running.is_none(), "the dead worker's lease has not lapsed yet: nobody else can take the job");

    // A new worker (this process) takes over once the 3-second lease has lapsed.
    let token = CancellationToken::new();
    let task = {
        let (ctx, token) = (ctx.clone(), token.clone());
        let w = worker(mta.addr);
        tokio::spawn(async move { Box::new(w).run(ctx, token).await })
    };
    let mut done = false;
    for _ in 0..300 {
        let n: i64 = done_jobs(&fx).await;
        if n == 1 {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    token.cancel();
    let _ = task.await;
    assert!(done, "the job finished");
    assert!(mta.messages().len() >= 2, "at-least-once: the remote has it from the dead worker and from its successor");
    let inbox = rusty_store::Mailbox::open_with(store.repos().mailbox.clone(), ctx.blobs.clone(), alice).await.unwrap();
    assert!(inbox.list_folder(rusty_core::FolderId(1), 0, 10).await.unwrap().is_empty(), "no bounce for a delivered message");
}

async fn done_jobs(fx: &Fixtures) -> i64 {
    fx.scalar("SELECT count(*) FROM jobs WHERE state = 'done'", &[]).await
}
