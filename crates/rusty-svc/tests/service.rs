//! `Context` and the service runner; the seam traits are wired by default and replaceable.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use rusty_core::{Error, MailboxId};
use rusty_store::Repos;
use rusty_svc::{
    run_services, Context, GcService, QuotaPolicy, RateLimiter, Service, ServiceEvent, ServiceEvents, TlsProvider,
    Verdict,
};
use tokio_util::sync::CancellationToken;

async fn ctx(p: &Parts) -> Context {
    let cfg = Arc::new(dev_config(p.dir.path()));
    let tls = TlsProvider::from_config(&cfg.tls, "localhost", Arc::new(rusty_svc::FileSecretSource::new())).unwrap();
    let repos: &Repos = p.store.repos();
    Context::new(repos, p.blobs.clone(), cfg, tls).await.unwrap()
}

// backend-neutral: the store only builds the Context; the seams under test are in-memory
#[tokio::test]
async fn the_default_seams_are_wired_through_their_traits() {
    let p = parts().await;
    let c = ctx(&p).await;

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
        assert_eq!(r.check(ip, "a@x.test"), Verdict::Allowed);
        r.record_failure(ip, "a@x.test");
    }
    assert!(matches!(r.check(ip, "a@x.test"), Verdict::Blocked { .. }));

    // ServiceEvents: replaceable; the default tracing implementation accepts every event.
    c.events.emit(ServiceEvent::AuthResult { service: "t", peer: "192.0.2.9:1".parse().unwrap(), ok: false });

    // Hub: opens a mailbox through the repository trait only.
    let dom = c.directory.create_domain("ctx.test").await.unwrap();
    let acc = c.directory.create_account("alice", dom, None, None).await.unwrap();
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

// backend-neutral: the store only builds the Context; the seams under test are in-memory
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

// backend-neutral: the store only builds the Context; the runner under test never queries it
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

// backend-neutral: the store only builds the Context; the runner under test never queries it
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

// backend-neutral: SQLite store used only to build the Context; GC on both backends is covered by rusty-store
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
