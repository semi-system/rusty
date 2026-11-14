//! What the process does with the change feed (migration 0017): announcements of other processes
//! reach the hub (IMAP IDLE) and the job waiter (outbound worker); a resync wakes everything that
//! follows something, because notifications sent while the feed was away are gone.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use rusty_core::MailboxId;
use rusty_store::{ChangeFeed, ChangeSink};
use rusty_svc::{ChangeFanout, ChangeFeedService, Context, MailboxHub, Service, TlsProvider};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

async fn hub(p: &Parts) -> MailboxHub {
    MailboxHub::new(p.store.repos().mailbox.clone(), p.blobs.clone(), rusty_store::MailboxLimits::default())
}

async fn ctx(p: &Parts) -> Context {
    let cfg = Arc::new(dev_config(p.dir.path()));
    let tls = TlsProvider::from_config(&cfg.tls, "localhost", Arc::new(rusty_svc::FileSecretSource::new())).unwrap();
    Context::new(p.store.repos(), p.blobs.clone(), cfg, tls).await.unwrap()
}

async fn woken(n: &Notify) -> bool {
    tokio::time::timeout(Duration::from_millis(300), n.notified()).await.is_ok()
}

#[tokio::test]
async fn poke_all_wakes_every_subscriber_and_leaves_the_sequence_alone() {
    let p = parts().await;
    let hub = hub(&p).await;
    let (a, b) = (MailboxId(1), MailboxId(2));
    let (mut ra, mut rb) = (hub.subscribe(a), hub.subscribe(b));
    hub.notify(a, 7);
    ra.borrow_and_update();
    rb.borrow_and_update();
    assert!(!ra.has_changed().unwrap() && !rb.has_changed().unwrap());

    hub.poke_all();

    assert!(ra.has_changed().unwrap() && rb.has_changed().unwrap(), "every followed mailbox is woken");
    assert_eq!((*ra.borrow(), *rb.borrow()), (7, 0), "a poke carries no sequence");
    // A poke must not push the stored sequence up: the next real change is still heard.
    ra.borrow_and_update();
    hub.notify(a, 8);
    assert!(ra.has_changed().unwrap());
    assert_eq!(*ra.borrow(), 8);
}

#[tokio::test]
async fn the_fanout_delivers_a_mailbox_announcement_to_the_hub() {
    let p = parts().await;
    let hub = hub(&p).await;
    let mut rx = hub.subscribe(MailboxId(3));
    let fan = ChangeFanout::new(hub.clone());
    fan.mailbox_changed(MailboxId(3), 5);
    assert!(rx.has_changed().unwrap());
    assert_eq!(*rx.borrow_and_update(), 5);
    // A mailbox nobody follows costs nothing and creates no channel.
    fan.mailbox_changed(MailboxId(99), 1);
    assert_eq!(hub.channels(), 1);
}

#[tokio::test]
async fn the_fanout_wakes_the_job_waiter_only_for_queued_jobs() {
    let p = parts().await;
    let jobs = Arc::new(Notify::new());
    let fan = ChangeFanout::new(hub(&p).await).with_jobs(jobs.clone());
    assert!(!woken(&jobs).await, "nothing happened yet");
    fan.mailbox_changed(MailboxId(1), 1);
    assert!(!woken(&jobs).await, "a mailbox change is not a job");
    fan.jobs_queued();
    assert!(woken(&jobs).await);
}

#[tokio::test]
async fn a_resync_wakes_the_followed_mailboxes_and_the_job_waiter() {
    let p = parts().await;
    let hub = hub(&p).await;
    let jobs = Arc::new(Notify::new());
    let mut rx = hub.subscribe(MailboxId(4));
    let fan = ChangeFanout::new(hub.clone()).with_jobs(jobs.clone());
    rx.borrow_and_update();
    fan.resync();
    assert!(rx.has_changed().unwrap(), "IDLE sessions must look at their change log again");
    assert!(woken(&jobs).await, "the worker must look at the queue again");
}

/// A feed that follows a script: one announcement, then waits for the shutdown.
struct ScriptedFeed {
    ran: Mutex<u32>,
}

#[async_trait]
impl ChangeFeed for ScriptedFeed {
    async fn run(&self, sink: Arc<dyn ChangeSink>, shutdown: CancellationToken) -> rusty_core::Result<()> {
        *self.ran.lock().unwrap() += 1;
        sink.resync();
        sink.mailbox_changed(MailboxId(6), 11);
        shutdown.cancelled().await;
        Ok(())
    }
}

#[tokio::test]
async fn the_service_runs_the_feed_into_the_sink_and_stops_on_shutdown() {
    let p = parts().await;
    let c = ctx(&p).await;
    let hub = hub(&p).await;
    let mut rx = hub.subscribe(MailboxId(6));
    let feed = Arc::new(ScriptedFeed { ran: Mutex::new(0) });
    let token = CancellationToken::new();
    let service = ChangeFeedService::new(feed.clone(), Arc::new(ChangeFanout::new(hub)));
    assert_eq!(service.name(), "change-feed");
    let h = tokio::spawn(Box::new(service).run(c, token.clone()));

    tokio::time::timeout(Duration::from_secs(5), rx.changed()).await.unwrap().unwrap();
    assert_eq!(*rx.borrow(), 11);
    assert_eq!(*feed.ran.lock().unwrap(), 1);

    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), h).await.unwrap().unwrap().unwrap();
}
