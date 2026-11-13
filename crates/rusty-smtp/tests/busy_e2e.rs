//! A saturated or locked database never hangs a session: the delivery is answered `451 4.3.0`
//! within a bound, nothing is stored, and the very same message is accepted once the database is
//! free again (no loss, no duplicate). Postgres' table lock and a pool with no
//! connection left.

mod common;

use std::time::{Duration, Instant};

use common::{for_each_backend_short_waits, start, Client, Opts};

const MSG: &[u8] = b"From: Sender <sender@remote.example>\r\nTo: alice@example.net\r\nSubject: busy\r\n\r\nbody\r\n";

async fn send(addr: std::net::SocketAddr) -> (String, Duration) {
    let mut c = Client::hello(addr).await;
    let t = Instant::now();
    // The first refusal is the answer: the database may fail at MAIL, RCPT or the end of DATA.
    let mut r = c.cmd("MAIL FROM:<sender@remote.example>").await;
    if r.starts_with("250") {
        r = c.cmd("RCPT TO:<alice@example.net>").await;
    }
    if r.starts_with("250") {
        r = c.cmd("DATA").await;
    }
    if r.starts_with("354") {
        c.send(MSG).await;
        c.send(b".\r\n").await;
        r = c.reply().await;
    }
    let took = t.elapsed();
    let _ = c.cmd("QUIT").await;
    (r, took)
}

#[tokio::test]
async fn a_locked_database_gives_451_not_a_hang_and_the_retry_lands_once() {
    for_each_backend_short_waits(|b| async move {
        let (alice, _) = b.seed().await;
        let s = start(&b, Opts::default()).await;
        let lock = b.fx.hold_write_lock("mailboxes").await;
        let (r, took) = send(s.addr).await;
        assert!(r.starts_with("451 4.3.0"), "{}: {r}", b.name);
        assert!(took < Duration::from_secs(5), "{}: the answer took {took:?}", b.name);
        drop(lock);
        assert!(b.inbox(alice, &s.blobs).await.is_empty(), "nothing stored by the refused delivery");
        let (r, _) = send(s.addr).await;
        assert!(r.starts_with("250"), "{}: {r}", b.name);
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1, "exactly one copy after the retry");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn an_exhausted_pool_gives_451_not_a_hang() {
    for_each_backend_short_waits(|b| async move {
        let (alice, _) = b.seed().await;
        let s = start(&b, Opts::default()).await;
        let hold = b.fx.hold_pool(2).await;
        let (r, took) = send(s.addr).await;
        assert!(r.starts_with("451 4.3.0"), "{r}");
        assert!(took < Duration::from_secs(5), "{took:?}");
        drop(hold);
        let (r, _) = send(s.addr).await;
        assert!(r.starts_with("250"), "{r}");
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1);
        s.stop().await;
    })
    .await;
}
