//! The single place that announces mailbox changes: a mailbox opened through the hub wakes its
//! subscribers after every committed write, whoever wrote (SMTP delivery, IMAP, a later service).

mod common;

use std::time::Duration;

use common::*;

#[tokio::test]
async fn a_write_through_the_hub_wakes_subscribers_of_that_mailbox_only() {
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("n.test").await.unwrap();
        let alice = b.account_in(dom, "alice", "pw").await;
        let bob = b.account_in(dom, "bob", "pw").await;
        let ctx = b.ctx().await;
        let (ma, mb) = (b.mailbox(&ctx, alice).await, b.mailbox(&ctx, bob).await);
        let mut rx_a = ctx.hub.subscribe(ma.id());
        let mut rx_b = ctx.hub.subscribe(mb.id());
        deliver(&ma, 1, "hello", "x").await;
        tokio::time::timeout(Duration::from_secs(2), rx_a.changed()).await.unwrap().unwrap();
        assert!(*rx_a.borrow() >= 1, "{}", b.name);
        assert!(tokio::time::timeout(Duration::from_millis(150), rx_b.changed()).await.is_err(), "{}: other mailbox", b.name);
        // Flag changes and folder changes announce too, and a failed write does not.
        let seq = *rx_a.borrow();
        ma.create_folder(None, "F").await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), rx_a.changed()).await.unwrap().unwrap();
        assert!(*rx_a.borrow() > seq);
        let before = *rx_a.borrow();
        assert!(ma.create_folder(None, "F").await.is_err());
        assert_eq!(*rx_a.borrow(), before);
    })
    .await;
}
