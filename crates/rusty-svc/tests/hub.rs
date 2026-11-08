//! `MailboxHub`: change notification for IDLE; a slow or absent subscriber never blocks a notifier.

mod common;

use std::time::{Duration, Instant};

use rusty_core::MailboxId;
use rusty_svc::MailboxHub;

async fn hub() -> (MailboxHub, common::Parts) {
    let p = common::parts().await;
    let limits = rusty_store::MailboxLimits::default();
    (MailboxHub::new(p.store.repos().mailbox.clone(), p.blobs.clone(), limits), p)
}

// backend-neutral: the store is only a constructor argument; the notification code under test never queries it
#[tokio::test]
async fn notify_wakes_subscribers() {
    let (hub, _parts) = hub().await;
    let id = MailboxId(7);
    let mut a = hub.subscribe(id);
    let mut b = hub.subscribe(id);
    assert_eq!(*a.borrow(), 0);
    hub.notify(id, 3);
    tokio::time::timeout(Duration::from_secs(1), a.changed()).await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(1), b.changed()).await.unwrap().unwrap();
    assert_eq!(*a.borrow(), 3);
    assert_eq!(*b.borrow(), 3);
}

// backend-neutral: the store is only a constructor argument; the notification code under test never queries it
#[tokio::test]
async fn other_mailboxes_are_not_woken() {
    let (hub, _parts) = hub().await;
    let mut a = hub.subscribe(MailboxId(1));
    hub.notify(MailboxId(2), 9);
    assert!(tokio::time::timeout(Duration::from_millis(100), a.changed()).await.is_err());
}

// backend-neutral: the store is only a constructor argument; the notification code under test never queries it
#[tokio::test]
async fn sequence_numbers_never_go_backwards() {
    let (hub, _parts) = hub().await;
    let id = MailboxId(1);
    let rx = hub.subscribe(id);
    hub.notify(id, 10);
    hub.notify(id, 4);
    assert_eq!(*rx.borrow(), 10);
}

// backend-neutral: the store is only a constructor argument; the notification code under test never queries it
#[tokio::test]
async fn a_slow_subscriber_cannot_block_notifiers() {
    let (hub, _parts) = hub().await;
    let id = MailboxId(1);
    // Subscribed and never read; plus plenty of notifications.
    let slow = hub.subscribe(id);
    let t0 = Instant::now();
    for seq in 1..=200_000u64 {
        hub.notify(id, seq);
    }
    assert!(t0.elapsed() < Duration::from_secs(5), "notify blocked: {:?}", t0.elapsed());
    // The slow subscriber sees only the latest value when it finally looks.
    assert_eq!(*slow.borrow(), 200_000);
    // Notifying from many tasks at once does not deadlock either.
    let mut joins = Vec::new();
    for t in 0..8u64 {
        let h = hub.clone();
        joins.push(tokio::spawn(async move {
            for s in 0..5_000u64 {
                h.notify(id, 200_000 + t * 5_000 + s);
            }
        }));
    }
    for j in joins {
        tokio::time::timeout(Duration::from_secs(5), j).await.unwrap().unwrap();
    }
}

// backend-neutral: the store is only a constructor argument; the notification code under test never queries it
#[tokio::test]
async fn dropped_subscribers_do_not_leak_channels() {
    let (hub, _parts) = hub().await;
    for i in 0..5_000i64 {
        let rx = hub.subscribe(MailboxId(i));
        drop(rx);
        hub.notify(MailboxId(i), 1);
    }
    assert!(hub.channels() <= 1_100, "channels leaked: {}", hub.channels());
}
