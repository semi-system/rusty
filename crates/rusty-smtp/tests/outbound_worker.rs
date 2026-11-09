//! The outbound worker against loopback fake remote MTAs (see `common::fake_mta`), on every backend:
//! delivery, MX order and fallback, null MX, 4xx retries and greylisting, partial recipients, bounces
//! (and the no-backscatter rule), expiry, timeouts, TLS policies, DKIM signing, SMTPUTF8/IDN, size
//! limits, relay, per-domain concurrency, tenant queue cap and lease takeover.
mod common;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::fake_mta::*;
use common::*;
use rusty_core::config::{OutboundConfig, RelayConfig, RelayTls, TlsPolicy};
use rusty_core::{AccountId, Error};
use rusty_smtp::dkim::{generate_key, keyring, DkimAlgo};
use rusty_smtp::outbound::dial::Dialer;
use rusty_smtp::outbound::route::ConfigRoute;
use rusty_smtp::outbound::tls::RustlsUpgrader;
use rusty_smtp::outbound::worker::{OutboundWorker, WorkerDeps, WorkerOptions};
use rusty_smtp::outbound_queue::JobOutbound;
use rusty_smtp::resolve::{MxAnswer, MxHost, StaticDns};
use rusty_smtp::stack::DnsStack;
use rusty_store::BlobStore;
use rusty_svc::{Context, FileSecretSource, Service};
use tokio_util::sync::CancellationToken;

const MSG: &str = "From: alice@example.net\r\nTo: bob@remote.test\r\nSubject: hi\r\nMessage-ID: <1@example.net>\r\nDate: Mon, 09 Nov 2026 10:00:00 +0000\r\n\r\nhello bob\r\n.dot line\r\nlast\r\n";

fn fast_cfg() -> OutboundConfig {
    OutboundConfig {
        backoff_secs: vec![1],
        max_age_secs: 3600,
        connect_timeout_secs: 2,
        command_timeout_secs: 2,
        data_timeout_secs: 5,
        lease_secs: 60,
        ..OutboundConfig::default()
    }
}

fn dns_one(host: &str, ip: &str) -> StaticDns {
    let mut dns = StaticDns::default();
    dns.mx.insert("remote.test".into(), MxAnswer::Hosts(vec![MxHost { preference: 10, host: host.into() }]));
    dns.addrs.insert(host.into(), vec![ip.parse().unwrap()]);
    dns
}

struct Rig {
    b: Backend,
    ctx: Context,
    blobs: Arc<dyn BlobStore>,
    alice: AccountId,
    token: CancellationToken,
    task: Option<tokio::task::JoinHandle<rusty_core::Result<()>>>,
    dialer: Arc<MapDialer>,
    deps: WorkerDeps,
    opts: WorkerOptions,
}

async fn rig(b: Backend, dialer: Arc<MapDialer>, dns: StaticDns, cfg: OutboundConfig) -> Rig {
    let (alice, _bob) = b.seed().await;
    let (ctx, blobs) = make_context(&b).await;
    let stack = DnsStack::with_static(dns, HOST).unwrap();
    let route = ConfigRoute::from_config(&cfg, &FileSecretSource::new()).unwrap();
    let tls = RustlsUpgrader::new(cfg.ca_bundle.as_deref()).unwrap();
    let signer = keyring(&cfg, Arc::new(FileSecretSource::new()));
    let deps = WorkerDeps {
        resolver: stack.resolver.clone(),
        dialer: dialer.clone() as Arc<dyn Dialer>,
        tls: Arc::new(tls),
        route: Arc::new(route),
        signer,
    };
    let mut opts = WorkerOptions::new(cfg, HOST);
    opts.poll = Duration::from_millis(50);
    Rig { b, ctx, blobs, alice, token: CancellationToken::new(), task: None, dialer, deps, opts }
}

impl Rig {
    fn start(&mut self) {
        let worker = OutboundWorker::new(self.deps.clone(), self.opts.clone());
        let (ctx, token) = (self.ctx.clone(), self.token.clone());
        self.task = Some(tokio::spawn(async move { Box::new(worker).run(ctx, token).await }));
    }

    async fn stop(&mut self) {
        self.token.cancel();
        if let Some(t) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(10), t).await;
        }
    }

    async fn submit(&self, from: &str, rcpts: &[&str], msg: &str) {
        let blob = self.blobs.put(Bytes::from(msg.as_bytes().to_vec())).await.unwrap();
        let enq = JobOutbound::new(self.ctx.jobs.clone(), self.ctx.directory.clone());
        use rusty_svc::{OutboundEnqueue, OutboundMessage};
        enq.enqueue(OutboundMessage {
            blob,
            envelope_from: from.into(),
            recipients: rcpts.iter().map(|s| (*s).to_owned()).collect(),
            sender_domain: "example.net".into(),
        })
        .await
        .unwrap();
    }

    async fn jobs(&self, state: &str) -> i64 {
        self.b.fx.scalar(&format!("SELECT count(*) FROM jobs WHERE state = '{state}'"), &[]).await
    }

    async fn wait_done(&self, n: i64) {
        for _ in 0..400 {
            if self.jobs("done").await >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("jobs did not finish: done={} queued={} running={} dead={}", self.jobs("done").await, self.jobs("queued").await, self.jobs("running").await, self.jobs("dead").await);
    }

    async fn inbox(&self) -> Vec<String> {
        self.b.inbox(self.alice, &self.blobs).await.into_iter().map(|m| String::from_utf8_lossy(&m).into_owned()).collect()
    }
}

fn lines(m: &Received) -> String {
    String::from_utf8_lossy(&m.data).into_owned()
}

// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn delivers_byte_exact_over_plain_smtp_and_completes_the_job() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| Script::default()).await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), fast_cfg()).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        r.wait_done(1).await;
        let got = mta.messages();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].from, "alice@example.net");
        assert_eq!(got[0].rcpts, vec!["bob@remote.test"]);
        assert_eq!(got[0].helo, HOST);
        assert_eq!(lines(&got[0]), MSG, "dot-stuffing round trip, byte exact");
        assert!(r.inbox().await.is_empty(), "no bounce on success");
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn mx_preference_order_and_fallback_to_the_next_exchanger() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| Script::default()).await;
        // mx1 (preference 10) refuses connections, mx2 (20) works, mx0 (5, tried first) is unreachable too.
        let dialer = MapDialer::new(&[("192.0.2.20", mta.addr)]);
        let mut dns = StaticDns::default();
        dns.mx.insert(
            "remote.test".into(),
            MxAnswer::Hosts(vec![
                MxHost { preference: 20, host: "mx2.fake.test".into() },
                MxHost { preference: 10, host: "mx1.fake.test".into() },
                MxHost { preference: 5, host: "mx0.fake.test".into() },
            ]),
        );
        dns.addrs.insert("mx0.fake.test".into(), vec!["192.0.2.5".parse().unwrap()]);
        dns.addrs.insert("mx1.fake.test".into(), vec!["192.0.2.10".parse().unwrap()]);
        dns.addrs.insert("mx2.fake.test".into(), vec!["192.0.2.20".parse().unwrap()]);
        let mut r = rig(b, dialer.clone(), dns, fast_cfg()).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        r.wait_done(1).await;
        let order: Vec<String> = dialer.dialed().iter().map(IpAddr::to_string).collect();
        assert_eq!(order, ["192.0.2.5", "192.0.2.10", "192.0.2.20"], "lowest preference first, then fall through");
        assert_eq!(mta.messages().len(), 1);
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_domain_without_mx_uses_its_own_address_record() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| Script::default()).await;
        let dialer = MapDialer::new(&[("192.0.2.30", mta.addr)]);
        let mut dns = StaticDns::default();
        dns.mx.insert("remote.test".into(), MxAnswer::NoRecords);
        dns.addrs.insert("remote.test".into(), vec!["192.0.2.30".parse().unwrap()]);
        let mut r = rig(b, dialer, dns, fast_cfg()).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        r.wait_done(1).await;
        assert_eq!(mta.messages().len(), 1);
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_null_mx_is_a_permanent_failure_and_one_bounce_reaches_the_sender() {
    for_each_backend(|b| async move {
        let dialer = MapDialer::new(&[]);
        let mut dns = StaticDns::default();
        dns.mx.insert("remote.test".into(), MxAnswer::NullMx);
        let mut r = rig(b, dialer.clone(), dns, fast_cfg()).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        r.wait_done(1).await;
        assert!(dialer.dialed().is_empty(), "nothing is dialed for a null MX");
        let inbox = r.inbox().await;
        assert_eq!(inbox.len(), 1, "{inbox:?}");
        let dsn = &inbox[0];
        assert!(dsn.starts_with("Return-Path: <>\r\n"), "{dsn}");
        assert!(dsn.contains("Content-Type: multipart/report; report-type=delivery-status"), "{dsn}");
        assert!(dsn.contains("Final-Recipient: rfc822; bob@remote.test") && dsn.contains("Action: failed"), "{dsn}");
        assert!(dsn.contains("Subject: hi"), "the ORIGINAL headers are attached");
        assert!(!dsn.contains("hello bob"), "the body is not returned");
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_4xx_then_250_delivers_exactly_once_greylisting() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|n| {
            let mut s = Script::default();
            if n == 1 {
                s.rcpt = Box::new(|_| "450 4.2.0 greylisted, try again later".into());
            }
            s
        })
        .await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), fast_cfg()).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        r.wait_done(1).await;
        assert_eq!(mta.messages().len(), 1, "one delivery");
        assert!(mta.connection_count() >= 2, "the first try was deferred");
        assert!(r.inbox().await.is_empty(), "a deferral is not a bounce");
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn partial_failure_delivers_the_good_recipient_and_bounces_only_the_bad_one() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| {
            let mut s = Script::default();
            s.rcpt = Box::new(|a| if a.starts_with("nobody@") { "550 5.1.1 no such user".into() } else { "250 2.1.5 ok".into() });
            s
        })
        .await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), fast_cfg()).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test", "nobody@remote.test"], MSG).await;
        r.wait_done(1).await;
        let got = mta.messages();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].rcpts, vec!["bob@remote.test"]);
        let inbox = r.inbox().await;
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].contains("Final-Recipient: rfc822; nobody@remote.test") && !inbox[0].contains("Final-Recipient: rfc822; bob@"), "{}", inbox[0]);
        assert!(inbox[0].contains("Status: 5.1.1") && inbox[0].contains("550 5.1.1 no such user"), "{}", inbox[0]);
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_temporary_and_a_permanent_recipient_bounce_one_now_and_retry_the_other_without_resending() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|n| {
            let mut s = Script::default();
            s.rcpt = Box::new(move |a| {
                if a.starts_with("nobody@") {
                    "550 5.1.1 no such user".into()
                } else if a.starts_with("slow@") && n == 1 {
                    "451 4.7.1 greylisted".into()
                } else {
                    "250 2.1.5 ok".into()
                }
            });
            s
        })
        .await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), fast_cfg()).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test", "nobody@remote.test", "slow@remote.test"], MSG).await;
        r.wait_done(1).await;
        let got = mta.messages();
        let all: Vec<String> = got.iter().flat_map(|m| m.rcpts.clone()).collect();
        assert_eq!(all.iter().filter(|a| a.starts_with("bob@")).count(), 1, "bob is not sent to twice: {all:?}");
        assert_eq!(all.iter().filter(|a| a.starts_with("slow@")).count(), 1, "{all:?}");
        assert_eq!(r.inbox().await.len(), 1, "exactly one bounce, for nobody@");
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_5xx_after_data_is_one_bounce() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| {
            let mut s = Script::default();
            s.data_end = Box::new(|_| DataEnd::Reply("554 5.7.1 rejected as spam".into()));
            s
        })
        .await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), fast_cfg()).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        r.wait_done(1).await;
        let inbox = r.inbox().await;
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].contains("554 5.7.1 rejected as spam"), "{}", inbox[0]);
        assert_eq!(mta.messages().len(), 0);
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn no_backscatter_for_a_null_sender_or_a_foreign_envelope_sender() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| {
            let mut s = Script::default();
            s.rcpt = Box::new(|_| "550 5.1.1 no such user".into());
            s
        })
        .await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), fast_cfg()).await;
        r.start();
        r.submit("", &["bob@remote.test"], MSG).await;
        r.submit("forged@victim.example", &["bob@remote.test"], MSG).await;
        r.wait_done(2).await;
        assert!(r.inbox().await.is_empty(), "no bounce for a null sender or for an address that is not a local account");
        // Nothing was sent anywhere but to the failing remote: no connection to the forged domain exists.
        assert_eq!(r.b.count("jobs").await, 2);
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_message_older_than_max_age_is_bounced_with_4_4_7() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| {
            let mut s = Script::default();
            s.rcpt = Box::new(|_| "451 4.3.0 try later".into());
            s
        })
        .await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut cfg = fast_cfg();
        cfg.max_age_secs = 3;
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), cfg).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        r.wait_done(1).await;
        let inbox = r.inbox().await;
        assert_eq!(inbox.len(), 1, "exactly one bounce");
        assert!(inbox[0].contains("Status: 4.4.7"), "{}", inbox[0]);
        assert_eq!(mta.messages().len(), 0);
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_silent_server_times_out_and_the_job_is_kept_for_a_retry() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| Script { silent: true, ..Script::default() }).await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut cfg = fast_cfg();
        cfg.command_timeout_secs = 1;
        cfg.backoff_secs = vec![3600];
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), cfg).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        for _ in 0..100 {
            if r.b.fx.scalar("SELECT count(*) FROM jobs WHERE state = 'queued' AND run_after > 0 AND last_error LIKE '%timed out%'", &[]).await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(r.jobs("queued").await, 1, "deferred, not lost and not bounced");
        assert!(r.inbox().await.is_empty());
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_sender_domain_over_the_queue_cap_is_refused_at_enqueue() {
    for_each_backend(|b| async move {
        b.seed().await;
        let (ctx, blobs) = make_context(&b).await;
        let jobs = rusty_store::JobQueue::with_repo_and_limits(b.store.repos().jobs.clone(), 2, 2);
        let enq = JobOutbound::new(jobs, ctx.directory.clone());
        use rusty_svc::{OutboundEnqueue, OutboundMessage};
        let blob = blobs.put(Bytes::from_static(b"x")).await.unwrap();
        let m = || OutboundMessage { blob: blob.clone(), envelope_from: "alice@example.net".into(), recipients: vec!["a@remote.test".into()], sender_domain: "example.net".into() };
        enq.enqueue(m()).await.unwrap();
        enq.enqueue(m()).await.unwrap();
        assert!(matches!(enq.enqueue(m()).await, Err(Error::Quota(_))), "the tenant's pending-job cap applies");
    })
    .await;
}

#[tokio::test]
async fn at_most_the_per_domain_cap_of_sessions_run_at_once() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| Script { data_delay: Duration::from_millis(400), ..Script::default() }).await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut cfg = fast_cfg();
        cfg.max_concurrent_per_domain = 1;
        cfg.max_concurrent = 8;
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), cfg).await;
        r.start();
        for i in 0..3 {
            r.submit("alice@example.net", &[&format!("u{i}@remote.test")], MSG).await;
        }
        r.wait_done(3).await;
        assert_eq!(mta.messages().len(), 3);
        assert_eq!(mta.max_concurrent.load(std::sync::atomic::Ordering::SeqCst), 1, "the cap held");
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_worker_that_dies_loses_its_lease_and_another_worker_delivers_once() {
    for_each_backend(|b| async move {
        // First connection: accept the message, then never answer (the worker is "killed" there).
        let mta = FakeMta::start(|n| {
            let mut s = Script::default();
            if n == 1 {
                s.data_end = Box::new(|_| DataEnd::Hang);
            }
            s
        })
        .await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut cfg = fast_cfg();
        cfg.lease_secs = 3;
        cfg.data_timeout_secs = 30;
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), cfg).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        for _ in 0..100 {
            if mta.messages().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(mta.messages().len(), 1, "the first worker sent the message");
        let old = r.ctx.jobs.claim("probe", &["outbound.send"], 3).await.unwrap();
        assert!(old.is_none(), "the lease is still held while the worker lives");
        // kill -9 equivalent: the task is aborted without any chance to settle the job.
        r.task.take().unwrap().abort();
        let stale = rusty_store::JobFence { id: rusty_core::JobId(1), epoch: 1 };
        tokio::time::sleep(Duration::from_secs(4)).await;
        r.token = CancellationToken::new();
        r.start();
        r.wait_done(1).await;
        assert!(r.ctx.jobs.fence_check(stale).await.is_err(), "the old worker's fence fails");
        // At-least-once: the remote saw the message from the dead worker and again from the new one;
        // the point is that it was not lost and the sender got no bounce.
        assert!(mta.messages().len() >= 2);
        assert!(r.inbox().await.is_empty());
        r.stop().await;
    })
    .await;
}


// ------------------------------------------------------------------------------------------------ TLS

fn write_bundle(r: &tempfile::TempDir, pem: &str) -> std::path::PathBuf {
    let p = r.path().join("ca.pem");
    std::fs::write(&p, pem).unwrap();
    p
}

#[tokio::test]
async fn opportunistic_tls_is_used_even_with_an_untrusted_certificate() {
    for_each_backend(|b| async move {
        let pki = TestPki::new("mx.fake.test");
        let sel = pki.stranger.clone();
        let mta = FakeMta::start(move |_| Script { tls: Some(sel.clone()), ..Script::default() }).await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), fast_cfg()).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        r.wait_done(1).await;
        let got = mta.messages();
        assert_eq!(got.len(), 1);
        assert!(got[0].tls, "STARTTLS was negotiated although the certificate chain is not trusted");
        assert_eq!(lines(&got[0]), MSG);
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn opportunistic_falls_back_to_plain_text_when_the_tls_handshake_fails() {
    for_each_backend(|b| async move {
        let pki = TestPki::new("mx.fake.test");
        let sel = pki.server.clone();
        let mta = FakeMta::start(move |_| Script { tls: Some(sel.clone()), break_tls: true, ..Script::default() }).await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), fast_cfg()).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        r.wait_done(1).await;
        let got = mta.messages();
        assert_eq!(got.len(), 1);
        assert!(!got[0].tls, "delivered in clear after the handshake failed");
        assert_eq!(mta.connection_count(), 2);
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn required_tls_defers_when_the_server_does_not_offer_starttls() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| Script::default()).await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut cfg = fast_cfg();
        cfg.tls_policy.insert("remote.test".into(), TlsPolicy::Required);
        cfg.backoff_secs = vec![3600];
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), cfg).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        for _ in 0..100 {
            if r.b.fx.scalar("SELECT count(*) FROM jobs WHERE state = 'queued' AND last_error LIKE '%TLS is required%'", &[]).await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(r.jobs("queued").await, 1);
        assert!(mta.messages().is_empty(), "never sent in clear");
        assert!(r.inbox().await.is_empty(), "deferred, not bounced");
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn verified_tls_trusts_only_the_configured_ca_bundle() {
    for_each_backend(|b| async move {
        let pki = TestPki::new("mx.fake.test");
        let dir = tempfile::tempdir().unwrap();
        let (good, bad) = (pki.server.clone(), pki.stranger.clone());
        let ok = FakeMta::start(move |_| Script { tls: Some(good.clone()), ..Script::default() }).await;
        let wrong = FakeMta::start(move |_| Script { tls: Some(bad.clone()), ..Script::default() }).await;
        let dialer = MapDialer::new(&[("192.0.2.10", ok.addr), ("192.0.2.11", wrong.addr)]);
        let mut cfg = fast_cfg();
        cfg.ca_bundle = Some(write_bundle(&dir, &pki.ca_pem));
        cfg.tls_policy.insert("remote.test".into(), TlsPolicy::Verified);
        cfg.tls_policy.insert("other.test".into(), TlsPolicy::Verified);
        cfg.backoff_secs = vec![3600];
        let mut dns = dns_one("mx.fake.test", "192.0.2.10");
        // other.test is served by the same host name, but with a certificate from an unknown CA.
        dns.mx.insert("other.test".into(), MxAnswer::Hosts(vec![MxHost { preference: 10, host: "mx2.fake.test".into() }]));
        dns.addrs.insert("mx2.fake.test".into(), vec!["192.0.2.11".parse().unwrap()]);
        let mut r = rig(b, dialer, dns, cfg).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        r.wait_done(1).await;
        assert!(ok.messages()[0].tls, "chain and host name verified against the bundle");
        r.submit("alice@example.net", &["carol@other.test"], MSG).await;
        for _ in 0..100 {
            if r.b.fx.scalar("SELECT count(*) FROM jobs WHERE state = 'queued' AND last_error LIKE '%TLS%'", &[]).await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(r.jobs("queued").await, 1, "an untrusted certificate defers the mail");
        assert!(wrong.messages().is_empty());
        r.stop().await;
    })
    .await;
}

// ------------------------------------------------------------------------------------------------ DKIM, UTF-8, size

#[tokio::test]
async fn outgoing_mail_is_dkim_signed_for_the_sender_domain_and_verifies() {
    for_each_backend(|b| async move {
        let keys = tempfile::tempdir().unwrap();
        let k = generate_key(keys.path(), "example.net", "rusty", DkimAlgo::Rsa2048).unwrap();
        let mta = FakeMta::start(|_| Script::default()).await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut cfg = fast_cfg();
        cfg.dkim_dir = Some(keys.path().to_owned());
        let dns = dns_one("mx.fake.test", "192.0.2.10");
        let mut r = rig(b, dialer, dns.clone(), cfg).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        r.wait_done(1).await;
        let sent = mta.messages().remove(0);
        let text = lines(&sent);
        assert!(text.starts_with("DKIM-Signature: "), "{text}");
        assert!(text.ends_with(MSG), "the original follows the signature unchanged");
        // The receiving side verifies it with the published record.
        let mut remote_dns = dns;
        remote_dns.txt.insert("rusty._domainkey.example.net".into(), k.txt_value());
        let stack = DnsStack::with_static(remote_dns, "mx.fake.test").unwrap();
        let v = stack
            .checker
            .check(&rusty_smtp::authres::AuthInput { peer: "192.0.2.1".parse().unwrap(), helo: HOST, mail_from: "alice@example.net", message: &sent.data })
            .await;
        assert_eq!(v.dkim, "pass", "{v:?}");
        assert!(v.header_value.contains("header.d=example.net"), "d= is the From domain: {}", v.header_value);
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn idn_domains_use_a_labels_and_smtputf8_is_negotiated_for_utf8_local_parts() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| Script { extensions: vec!["8BITMIME".into(), "SMTPUTF8".into()], ..Script::default() }).await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut dns = StaticDns::default();
        dns.mx.insert("xn--bcher-kva.example".into(), MxAnswer::Hosts(vec![MxHost { preference: 10, host: "mx.fake.test".into() }]));
        dns.addrs.insert("mx.fake.test".into(), vec!["192.0.2.10".parse().unwrap()]);
        let mut r = rig(b, dialer, dns, fast_cfg()).await;
        r.start();
        let body = "From: alice@example.net\r\nTo: j@b\u{fc}cher.example\r\nSubject: gr\u{fc}\u{df}e\r\nMessage-ID: <2@example.net>\r\n\r\nSch\u{f6}ne Gr\u{fc}\u{df}e\r\n";
        r.submit("alice@example.net", &["j\u{f6}rg@b\u{fc}cher.example"], body).await;
        r.wait_done(1).await;
        let got = mta.messages();
        assert_eq!(got.len(), 1);
        assert!(got[0].rcpt_lines[0].contains("RCPT TO:<j\u{f6}rg@xn--bcher-kva.example>"), "{:?}", got[0].rcpt_lines);
        assert!(got[0].mail_from_line.contains("SMTPUTF8") && got[0].mail_from_line.contains("BODY=8BITMIME"), "{}", got[0].mail_from_line);
        assert_eq!(lines(&got[0]), body, "8-bit body byte exact");
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn utf8_without_remote_support_bounces_instead_of_mangling() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| Script::default()).await; // offers neither SMTPUTF8 nor 8BITMIME
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), fast_cfg()).await;
        r.start();
        r.submit("alice@example.net", &["j\u{f6}rg@remote.test"], "From: alice@example.net\r\nSubject: x\r\n\r\nplain\r\n").await;
        r.submit("alice@example.net", &["bob@remote.test"], "From: alice@example.net\r\nSubject: x\r\n\r\nGr\u{fc}\u{df}e\r\n").await;
        r.wait_done(2).await;
        assert!(mta.messages().iter().all(|m| !m.rcpts.iter().any(|a| a.contains('\u{f6}'))));
        let inbox = r.inbox().await;
        assert_eq!(inbox.len(), 2, "{inbox:?}");
        assert!(inbox.iter().any(|m| m.contains("Status: 5.6.7")), "SMTPUTF8 missing");
        assert!(inbox.iter().any(|m| m.contains("Status: 5.6.1")), "8BITMIME missing");
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_message_over_the_remote_size_limit_is_bounced() {
    for_each_backend(|b| async move {
        let mta = FakeMta::start(|_| Script { extensions: vec!["SIZE 100".into()], ..Script::default() }).await;
        let dialer = MapDialer::new(&[("192.0.2.10", mta.addr)]);
        let mut r = rig(b, dialer, dns_one("mx.fake.test", "192.0.2.10"), fast_cfg()).await;
        r.start();
        r.submit("alice@example.net", &["bob@remote.test"], MSG).await;
        r.wait_done(1).await;
        assert!(mta.messages().is_empty());
        assert!(r.inbox().await[0].contains("Status: 5.3.4"));
        r.stop().await;
    })
    .await;
}

// ------------------------------------------------------------------------------------------------ relay

fn relay_cfg(dir: &tempfile::TempDir, pki: &TestPki, tls: RelayTls) -> OutboundConfig {
    use std::os::unix::fs::PermissionsExt;
    let pw = dir.path().join("relay.pw");
    std::fs::write(&pw, "s3cret\n").unwrap();
    std::fs::set_permissions(&pw, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut cfg = fast_cfg();
    cfg.ca_bundle = Some(write_bundle(dir, &pki.ca_pem));
    cfg.relay = Some(RelayConfig { host: "relay.fake.test".into(), port: 587, tls, username: Some("mailer".into()), password_file: Some(pw) });
    cfg.backoff_secs = vec![3600];
    cfg
}

#[tokio::test]
async fn a_relay_gets_all_remote_mail_with_credentials_over_verified_tls() {
    for_each_backend(|b| async move {
        let pki = TestPki::new("relay.fake.test");
        let dir = tempfile::tempdir().unwrap();
        let sel = pki.server.clone();
        let relay = FakeMta::start(move |_| Script { tls: Some(sel.clone()), require_auth: Some(("mailer".into(), "s3cret".into())), ..Script::default() }).await;
        let dialer = MapDialer::new(&[("192.0.2.50", relay.addr)]);
        let mut dns = StaticDns::default();
        dns.addrs.insert("relay.fake.test".into(), vec!["192.0.2.50".parse().unwrap()]);
        let mut r = rig(b, dialer, dns, relay_cfg(&dir, &pki, RelayTls::Starttls)).await;
        r.start();
        r.submit("alice@example.net", &["bob@anywhere.test"], MSG).await;
        r.wait_done(1).await;
        let got = relay.messages();
        assert_eq!(got.len(), 1);
        assert!(got[0].tls);
        assert_eq!(got[0].auth.as_deref(), Some("|mailer|s3cret"));
        assert_eq!(got[0].rcpts, vec!["bob@anywhere.test"]);
        r.stop().await;
    })
    .await;
}

#[tokio::test]
async fn relay_credentials_are_never_sent_without_tls() {
    for_each_backend(|b| async move {
        let pki = TestPki::new("relay.fake.test");
        let dir = tempfile::tempdir().unwrap();
        // The relay does not offer STARTTLS: the worker must not send the password.
        let relay = FakeMta::start(|_| Script { require_auth: Some(("mailer".into(), "s3cret".into())), ..Script::default() }).await;
        let dialer = MapDialer::new(&[("192.0.2.50", relay.addr)]);
        let mut dns = StaticDns::default();
        dns.addrs.insert("relay.fake.test".into(), vec!["192.0.2.50".parse().unwrap()]);
        let mut r = rig(b, dialer, dns, relay_cfg(&dir, &pki, RelayTls::Starttls)).await;
        r.start();
        r.submit("alice@example.net", &["bob@anywhere.test"], MSG).await;
        for _ in 0..100 {
            if r.jobs("queued").await == 1 && relay.connection_count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(relay.messages().is_empty());
        assert_eq!(r.jobs("queued").await, 1);
        r.stop().await;
    })
    .await;
}
