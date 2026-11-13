//! A locked or exhausted database never hangs an IMAP session: the write is answered
//! `NO [UNAVAILABLE]` within a bound and the same command works once the database is free.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{for_each_backend_short_waits, message, Conn};

async fn append(c: &mut Conn, tag: &str, subject: &str) -> (String, Duration) {
    let msg = message(subject, "x");
    let t = Instant::now();
    c.send(&format!("{tag} APPEND INBOX {{{}}}\r\n", msg.len())).await;
    let first = c.line().await;
    if first.starts_with('+') {
        c.send(&format!("{msg}\r\n")).await;
        return (c.line().await, t.elapsed());
    }
    (first, t.elapsed())
}

#[tokio::test]
async fn a_locked_or_exhausted_database_gives_no_unavailable_not_a_hang() {
    for_each_backend_short_waits(|b, fx| async move {
        b.account("alice@a.test", "pw").await;
        let ctx = b.ctx().await;
        let h = Arc::new(rusty_imap::ImapHandler::new(ctx));
        let mut c = Conn::open(h, true);
        c.line().await;
        c.login("alice@a.test", "pw").await;

        let lock = fx.hold_write_lock("mailboxes").await;
        let (r, took) = append(&mut c, "a1", "locked").await;
        assert!(r.starts_with("a1 NO [UNAVAILABLE]"), "{}: {r}", b.name);
        assert!(took < Duration::from_secs(5), "{}: {took:?}", b.name);
        drop(lock);

        let hold = fx.hold_pool(2).await;
        let (r, took) = append(&mut c, "a2", "pool").await;
        assert!(r.starts_with("a2 NO [UNAVAILABLE]"), "{r}");
        assert!(took < Duration::from_secs(5), "{took:?}");
        drop(hold);
        let (r, _) = append(&mut c, "a3", "free").await;
        assert!(r.starts_with("a3 OK"), "{}: {r}", b.name);
    })
    .await;
}
