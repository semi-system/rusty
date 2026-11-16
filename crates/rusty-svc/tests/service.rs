//! `Context` and the service runner; the seam traits are wired by default and replaceable.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use rusty_core::{Error, MailboxId};
use rusty_directory::Directory;
use rusty_store::Repos;
use rusty_svc::{
    run_services, Context, GcService, QuotaPolicy, RateLimiter, Service, ServiceEvent, ServiceEvents, TlsProvider,
    Verdict,
};
use tokio_util::sync::CancellationToken;

async fn ctx(p: &Parts) -> Context {
    ctx_over(p, p.store.repos()).await
}

/// A context over the given repositories: the operator's (`p.seed_store`) for a test of the context's own seams, which no one service role is responsible for.
async fn ctx_over(p: &Parts, repos: &Repos) -> Context {
    let cfg = Arc::new(dev_config(p.dir.path()));
    let tls = TlsProvider::from_config(&cfg.tls, "localhost", Arc::new(rusty_svc::FileSecretSource::new())).unwrap();
    Context::new(repos, p.blobs.clone(), cfg, tls).await.unwrap()
}

#[tokio::test]
async fn the_default_seams_are_wired_through_their_traits() {
    let p = parts().await;
    let c = ctx_over(&p, p.seed_store.repos()).await;

    // QuotaPolicy: size cap from the configuration, reached through the trait object.
    let q: &dyn QuotaPolicy = &*c.quota;
    let acct = rusty_core::AccountId(1);
    q.check_append(acct, 1024).unwrap();
    match q.check_append(acct, c.cfg.max_message_bytes + 1) {
        Err(Error::Quota(_)) => {}
        other => panic!("{other:?}"),
    }

    // RateLimiter: the in-memory auth throttle, limits from [limits].
    let r: &dyn RateLimiter = &*c.rate;
    let ip = "192.0.2.9".parse().unwrap();
    for _ in 0..c.cfg.limits.auth_failures_max {
        assert_eq!(r.check(ip, "a@x.test").await, Verdict::Allowed);
        r.record_failure(ip, "a@x.test").await;
    }
    assert!(matches!(r.check(ip, "a@x.test").await, Verdict::Blocked { .. }));

    // ServiceEvents: replaceable; the default tracing implementation accepts every event.
    c.events.emit(ServiceEvent::AuthResult { service: "t", peer: "192.0.2.9:1".parse().unwrap(), ok: false });

    // Hub: opens a mailbox through the repository trait only.
    let seed = Directory::with_repo(p.seed_store.repos().directory.clone());
    let dom = seed.create_domain("ctx.test").await.unwrap();
    let acc = seed.create_account("alice", dom, None, None).await.unwrap();
    let mb = c.hub.open_for(acc).await.unwrap();
    assert_eq!(c.hub.open_for(rusty_core::AccountId(9999)).await.err().map(|e| matches!(e, Error::NotFound(_))), Some(true));
    // and the hub's notifications are keyed by that mailbox
    let mut rx = c.hub.subscribe(mb.id());
    c.hub.notify(mb.id(), 1);
    rx.changed().await.unwrap();
    let _ = MailboxId(0);

    // Authenticator: wired to the directory (an unknown identity fails, it does not error).
    assert_eq!(c.auth.verify("nobody@ctx.test", "pw").await.unwrap(), None);
}

#[tokio::test]
async fn seams_can_be_replaced() {
    struct Deny;
    impl QuotaPolicy for Deny {
        fn check_append(&self, _: rusty_core::AccountId, _: u64) -> rusty_core::Result<()> {
            Err(Error::Quota("denied by test policy".into()))
        }
    }
    #[derive(Default)]
    struct Rec(Mutex<usize>);
    impl ServiceEvents for Rec {
        fn emit(&self, _: ServiceEvent<'_>) {
            *self.0.lock().unwrap() += 1;
        }
    }
    let p = parts().await;
    let rec = Arc::new(Rec::default());
    let c = ctx(&p).await.with_quota(Arc::new(Deny)).with_events(rec.clone());
    assert!(c.quota.check_append(rusty_core::AccountId(1), 1).is_err());
    c.events.emit(ServiceEvent::AuthResult { service: "t", peer: "192.0.2.9:1".parse().unwrap(), ok: true });
    assert_eq!(*rec.0.lock().unwrap(), 1);
}

struct Probe {
    name: &'static str,
    outcome: Outcome,
    stopped: Arc<Mutex<Vec<&'static str>>>,
}
enum Outcome {
    WaitForShutdown,
    Fail,
    Panic,
}
#[async_trait]
impl Service for Probe {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn run(self: Box<Self>, _ctx: Context, shutdown: CancellationToken) -> rusty_core::Result<()> {
        match self.outcome {
            Outcome::WaitForShutdown => {
                shutdown.cancelled().await;
                self.stopped.lock().unwrap().push(self.name);
                Ok(())
            }
            Outcome::Fail => Err(Error::storage("boom")),
            Outcome::Panic => panic!("service bug"),
        }
    }
}

#[tokio::test]
async fn the_runner_stops_everything_when_one_service_fails_or_panics() {
    for outcome in [Outcome::Fail, Outcome::Panic] {
        let p = parts().await;
        let c = ctx(&p).await;
        let stopped = Arc::new(Mutex::new(vec![]));
        let services: Vec<Box<dyn Service>> = vec![
            Box::new(Probe { name: "a", outcome: Outcome::WaitForShutdown, stopped: stopped.clone() }),
            Box::new(Probe { name: "b", outcome, stopped: stopped.clone() }),
            Box::new(Probe { name: "c", outcome: Outcome::WaitForShutdown, stopped: stopped.clone() }),
        ];
        let token = CancellationToken::new();
        let r = tokio::time::timeout(Duration::from_secs(5), run_services(services, c, token.clone())).await.unwrap();
        assert!(r.is_err(), "a failed service must fail the run");
        assert!(token.is_cancelled(), "the others were told to stop");
        let mut s = stopped.lock().unwrap().clone();
        s.sort();
        assert_eq!(s, vec!["a", "c"], "healthy services stopped cleanly");
    }
}

#[tokio::test]
async fn the_runner_returns_ok_after_a_requested_shutdown() {
    let p = parts().await;
    let c = ctx(&p).await;
    let stopped = Arc::new(Mutex::new(vec![]));
    let services: Vec<Box<dyn Service>> =
        vec![Box::new(Probe { name: "a", outcome: Outcome::WaitForShutdown, stopped: stopped.clone() })];
    let token = CancellationToken::new();
    let t2 = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        t2.cancel();
    });
    tokio::time::timeout(Duration::from_secs(5), run_services(services, c, token)).await.unwrap().unwrap();
    assert_eq!(*stopped.lock().unwrap(), vec!["a"]);
}

#[tokio::test]
async fn the_gc_service_runs_a_pass_through_the_gc_facade_and_stops_on_shutdown() {
    let p = parts().await;
    let c = ctx(&p).await;
    let report = GcService::run_once(&c).await.unwrap();
    assert_eq!(report.errors, 0);
    let token = CancellationToken::new();
    let h = tokio::spawn(Box::new(GcService::new(Duration::from_millis(50))).run(c, token.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), h).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn the_gc_pass_collects_every_blob_store_and_a_store_that_cannot_be_opened_is_counted_not_fatal() {
    let p = parts().await;
    let c = ctx(&p).await;
    let dir = Directory::with_repo(p.seed_store.repos().directory.clone());
    let d = dir.create_domain("tenant.test").await.unwrap();
    // A store row whose settings cannot be opened (a directory name that is a path): the pass skips it, counts one error and still runs the other stores.
    p.seed_store.repos().blob_stores.create_for_domain(d, "fs", r#"{"dir":"../escape"}"#).await.unwrap();
    let report = GcService::run_once(&c).await.unwrap();
    assert_eq!(report.errors, 1, "the unopenable store is counted");
    // With a good store the pass is clean.
    let d2 = dir.create_domain("good.test").await.unwrap();
    p.seed_store.repos().blob_stores.create_for_domain(d2, "fs", r#"{"dir":"good"}"#).await.unwrap();
    let again = GcService::run_once(&c).await.unwrap();
    assert_eq!(again.errors, 1, "only the unopenable store is counted; the good one collects cleanly");
}

#[tokio::test]
async fn the_upkeep_pass_makes_the_missing_copy_of_a_pool_and_skips_a_store_that_is_not_ready() {
    use rusty_store::{FsStoreOpener, KindOpener, Mailbox, PoolOpener};
    let p = parts().await;
    let cfg = Arc::new(dev_config(p.dir.path()));
    let tls = TlsProvider::from_config(&cfg.tls, "localhost", Arc::new(rusty_svc::FileSecretSource::new())).unwrap();
    let repos: &Repos = p.store.repos();
    let backends_dir = p.dir.path().join("backends");
    let fs = Arc::new(FsStoreOpener::new(backends_dir.clone()));
    let opener = KindOpener::new().with_pool(Arc::new(PoolOpener::new(repos.blob_pool.clone(), fs)));
    let c = Context::new_with_opener(repos, p.blobs.clone(), cfg, tls, Arc::new(opener)).await.unwrap();

    let dir = Directory::with_repo(p.seed_store.repos().directory.clone());
    let d = dir.create_domain("pool.test").await.unwrap();
    let store = p.seed_store.repos().blob_stores.create_for_domain(d, "pool", r#"{"copies":2,"placement":"ordered"}"#).await.unwrap();
    for n in ["a", "b"] {
        let id = p.seed_store.repos().blob_pool.add_backend(store, n, "fs", &format!(r#"{{"dir":"{n}"}}"#), 100).await.unwrap();
        p.seed_store.repos().blob_pool.record_backend_check(id, "ok", None).await.unwrap();
    }
    // A second pool that is not ready (its backend never passed a check): skipped, nothing fails.
    let d2 = dir.create_domain("notready.test").await.unwrap();
    let unready = p.seed_store.repos().blob_stores.create_for_domain(d2, "pool", "{}").await.unwrap();
    p.seed_store.repos().blob_pool.add_backend(unready, "x", "fs", r#"{"dir":"x"}"#, 100).await.unwrap();

    let acc = dir.create_account("alice", d, None, None).await.unwrap();
    let mb = Mailbox::open_with(p.seed_store.repos().mailbox.clone(), c.stores.clone(), acc).await.unwrap();
    let m = mb.append(rusty_core::FolderId(1), bytes::Bytes::from_static(b"Subject: pool\r\nMessage-ID: <p@x>\r\n\r\nbody\r\n"), Default::default()).await.unwrap();
    let blob = mb.get_raw_ref(m.id).await.unwrap().blob;
    let on_b = backends_dir.join("b").join(&blob.as_str()[0..2]).join(&blob.as_str()[2..4]).join(blob.as_str());
    assert!(on_b.exists(), "a mirror: both copies were written");
    // The copy on `b` is lost (and its record with it): the upkeep pass makes it again.
    std::fs::remove_file(&on_b).unwrap();
    let b_id = repos.blob_pool.backends(store).await.unwrap().into_iter().find(|x| x.name == "b").unwrap().id;
    p.seed_store.repos().blob_pool.remove_location(store, &blob, b_id).await.unwrap();
    assert_eq!(rusty_svc::MaintainService::run_once(&c).await.unwrap(), 1);
    assert!(on_b.exists(), "the copy was made again");
    assert_eq!(repos.blob_pool.locations(store, &blob).await.unwrap().len(), 2);
    assert_eq!(rusty_svc::MaintainService::run_once(&c).await.unwrap(), 0, "nothing left to do");
}
