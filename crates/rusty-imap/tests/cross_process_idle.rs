//! IMAP IDLE across processes. `smtp_wakes_idle.rs` proves the in-process path (one `Context`, one
//! hub). With one process per service the SMTP and the IMAP side share nothing in memory, so the
//! database announces the delivery (migration 0017) and the IMAP side hears it through the change feed.
//! Here the two sides are two `Context`s, each with its OWN hub, over one database.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use rusty_core::MailboxId;
use rusty_smtp::inbound::{InboundHandler, InboundOptions};
use rusty_store::fixtures::Fixtures;
use rusty_store::ChangeSink;
use rusty_svc::{BoxedStream, ChangeFanout, SessionHandler};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

async fn smtp_deliver(ctx: &rusty_svc::Context, to: &str, subject: &str) {
    let h = Arc::new(InboundHandler::new(ctx, InboundOptions::default()));
    let (mut client, server) = tokio::io::duplex(1 << 16);
    let task = tokio::spawn(async move {
        let s: BoxedStream = Box::new(server);
        h.handle(s, peer(false), CancellationToken::new()).await;
    });
    let mut buf = vec![0u8; 4096];
    let _ = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf)).await;
    let script = format!(
        "EHLO c.example\r\nMAIL FROM:<s@remote.example>\r\nRCPT TO:<{to}>\r\nDATA\r\nFrom: s@remote.example\r\nTo: {to}\r\nSubject: {subject}\r\n\r\nbody\r\n.\r\nQUIT\r\n"
    );
    client.write_all(script.as_bytes()).await.unwrap();
    let mut all = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await;
    let text = String::from_utf8_lossy(&all);
    assert!(text.contains("250 2.0.0"), "{text}");
    let _ = task.await;
}

/// The IMAP process's sink (the real fanout into its hub) that also tells the test when the feed
/// has subscribed and when it lost its connection.
struct Watch {
    inner: ChangeFanout,
    subscribed: Arc<Notify>,
    lost: Arc<Notify>,
    resyncs: Mutex<u32>,
}

impl ChangeSink for Watch {
    fn mailbox_changed(&self, mailbox: MailboxId, seq: i64) {
        self.inner.mailbox_changed(mailbox, seq);
    }
    fn jobs_queued(&self) {
        self.inner.jobs_queued();
    }
    fn resync(&self) {
        *self.resyncs.lock().unwrap() += 1;
        self.inner.resync();
        self.subscribed.notify_waiters();
        self.subscribed.notify_one();
    }
    fn lost(&self) {
        self.inner.lost();
        self.lost.notify_one();
    }
}

struct Running {
    watch: Arc<Watch>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<rusty_core::Result<()>>,
}

async fn start_feed(b: &Backend, imap_ctx: &rusty_svc::Context) -> Running {
    let watch = Arc::new(Watch {
        inner: ChangeFanout::new(imap_ctx.hub.clone()),
        subscribed: Arc::new(Notify::new()),
        lost: Arc::new(Notify::new()),
        resyncs: Mutex::new(0),
    });
    let stop = CancellationToken::new();
    let feed = b.store.change_feed();
    let task = tokio::spawn({
        let (watch, stop) = (watch.clone(), stop.clone());
        async move { feed.run(watch, stop).await }
    });
    tokio::time::timeout(Duration::from_secs(10), watch.subscribed.notified()).await.expect("the feed never subscribed");
    Running { watch, stop, task }
}

impl Running {
    async fn stop(self) {
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.task).await.expect("the feed must stop").unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_feed_a_delivery_in_another_context_does_not_wake_idle() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw").await;
        let imap_ctx = b.ctx().await;
        let smtp_ctx = b.ctx().await;
        let mut c = Conn::open(handler(imap_ctx), true);
        c.line().await;
        c.login("alice@a.test", "pw").await;
        c.ok("SELECT INBOX").await;
        c.send("i1 IDLE\r\n").await;
        assert_eq!(c.line().await, "+ idling");

        smtp_deliver(&smtp_ctx, "alice@a.test", "other process").await;

        // Control: the two hubs share nothing, and the IDLE poll is 30 s, so nothing arrives. This is what the
        // feed is for; if this assertion ever fails, the next test no longer proves anything.
        let early = tokio::time::timeout(Duration::from_secs(2), c.line()).await;
        assert!(early.is_err(), "the IDLE session heard a delivery of another context without any feed: {early:?}");
        c.send("DONE\r\n").await;
        assert_eq!(c.line().await, "* 1 EXISTS", "{}", b.name);
        assert!(c.line().await.starts_with("i1 OK"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_feed_a_delivery_in_another_context_wakes_idle() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw").await;
        let imap_ctx = b.ctx().await;
        let smtp_ctx = b.ctx().await;
        let feed = start_feed(&b, &imap_ctx).await;
        let mut c = Conn::open(handler(imap_ctx), true);
        c.line().await;
        c.login("alice@a.test", "pw").await;
        c.ok("SELECT INBOX").await;
        c.send("i1 IDLE\r\n").await;
        assert_eq!(c.line().await, "+ idling");

        let t0 = Instant::now();
        smtp_deliver(&smtp_ctx, "alice@a.test", "other process").await;
        let line = tokio::time::timeout(Duration::from_secs(5), c.line()).await.expect("IDLE was not woken by the feed");
        assert_eq!(line, "* 1 EXISTS", "{}", b.name);
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());

        c.send("DONE\r\n").await;
        assert!(c.line().await.starts_with("i1 OK"));
        feed.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_made_while_the_feed_was_down_is_found_after_the_resync() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw").await;
        let imap_ctx = b.ctx().await;
        let smtp_ctx = b.ctx().await;
        let feed = start_feed(&b, &imap_ctx).await;
        let mut c = Conn::open(handler(imap_ctx), true);
        c.line().await;
        c.login("alice@a.test", "pw").await;
        c.ok("SELECT INBOX").await;
        c.send("i1 IDLE\r\n").await;
        assert_eq!(c.line().await, "+ idling");

        // The server drops the feed's connection; the feed waits before it reconnects.
        let killed = Fixtures::of(&b.store)
            .scalar(
                "SELECT count(*) FROM (SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = current_database() AND pid <> pg_backend_pid() AND query LIKE 'LISTEN%') k",
                &[],
            )
            .await;
        assert_eq!(killed, 1, "exactly one LISTEN connection (the feed's) must have been terminated");
        tokio::time::timeout(Duration::from_secs(10), feed.watch.lost.notified()).await.expect("the loss was not reported");

        // Nobody is listening now: the notification of this delivery goes nowhere.
        smtp_deliver(&smtp_ctx, "alice@a.test", "while the feed was down").await;

        // The resync of the new subscription must wake the IDLE session, long before its 30 s poll.
        let line = tokio::time::timeout(Duration::from_secs(10), c.line()).await.expect("IDLE never heard of the delivery");
        assert_eq!(line, "* 1 EXISTS", "{}", b.name);
        assert!(*feed.watch.resyncs.lock().unwrap() >= 2, "the wake-up must come from a second subscription");

        c.send("DONE\r\n").await;
        assert!(c.line().await.starts_with("i1 OK"));
        feed.stop().await;
    })
    .await;
}
