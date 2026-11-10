//! IDLE: wake-up through the hub, the change-log poll for writers the hub does not see, DONE,
//! the per-account cap, the idle timeout and shutdown.

mod common;

use std::time::{Duration, Instant};

use common::*;
use rusty_imap::Limits;

async fn idling(b: &Backend, limits: Limits) -> (Conn, rusty_svc::Context, rusty_core::AccountId) {
    let acc = b.account("alice@a.test", "pw").await;
    let ctx = b.ctx().await;
    let mut c = Conn::open(handler_with(ctx.clone(), limits), true);
    c.line().await;
    c.login("alice@a.test", "pw").await;
    c.ok("SELECT INBOX").await;
    (c, ctx, acc)
}

#[tokio::test]
async fn a_delivery_wakes_an_idling_session_within_a_second_and_done_ends_it() {
    for_each_backend(|b| async move {
        let (mut c, ctx, acc) = idling(&b, Limits::default()).await;
        let mb = b.mailbox(&ctx, acc).await;
        c.send("i1 IDLE\r\n").await;
        assert_eq!(c.line().await, "+ idling");
        let t0 = Instant::now();
        deliver(&mb, 1, "pushed", "x").await;
        assert_eq!(c.line().await, "* 1 EXISTS");
        assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
        // A flag change by another session is pushed as well.
        let id = mb.list_folder(rusty_core::FolderId(1), 0, 10).await.unwrap()[0].id;
        mb.set_flags(id, rusty_core::Flags::SEEN).await.unwrap();
        assert_eq!(c.line().await, "* 1 FETCH (UID 1 FLAGS (\\Seen))");
        c.send("DONE\r\n").await;
        assert_eq!(c.line().await, "i1 OK IDLE terminated");
        // Normal commands resume.
        assert!(c.status("NOOP").await.contains("OK"));
    })
    .await;
}

#[tokio::test]
async fn a_writer_the_hub_cannot_see_is_found_by_the_poll() {
    for_each_backend(|b| async move {
        let limits = Limits { idle_poll: Duration::from_millis(300), ..Limits::default() };
        let (mut c, _ctx, acc) = idling(&b, limits).await;
        // A second hub (another process, `rustyctl`): its writes never reach the session's hub.
        let other = b.ctx().await;
        let mb = b.mailbox(&other, acc).await;
        c.send("i1 IDLE\r\n").await;
        assert_eq!(c.line().await, "+ idling");
        deliver(&mb, 1, "foreign", "x").await;
        assert_eq!(c.line().await, "* 1 EXISTS");
        c.send("DONE\r\n").await;
        assert!(c.line().await.starts_with("i1 OK"));
    })
    .await;
}

#[tokio::test]
async fn idle_is_capped_per_account_and_the_slot_is_freed() {
    for_each_backend(|b| async move {
        let limits = Limits { idle_per_account: 2, ..Limits::default() };
        let acc = b.account("alice@a.test", "pw").await;
        let _ = acc;
        let ctx = b.ctx().await;
        let h = handler_with(ctx, limits);
        let mut conns = Vec::new();
        for _ in 0..3 {
            let mut c = Conn::open(h.clone(), true);
            c.line().await;
            c.login("alice@a.test", "pw").await;
            conns.push(c);
        }
        for c in conns.iter_mut().take(2) {
            c.send("i IDLE\r\n").await;
            assert_eq!(c.line().await, "+ idling");
        }
        let third = &mut conns[2];
        let st = third.status("IDLE").await;
        assert!(st.contains("NO [LIMIT]"), "{st}");
        // Ending one IDLE frees a slot.
        conns[0].send("DONE\r\n").await;
        assert!(conns[0].line().await.starts_with("i OK"));
        let third = &mut conns[2];
        third.send("j IDLE\r\n").await;
        assert_eq!(third.line().await, "+ idling");
        // So does a closed connection.
        drop(conns.remove(1));
    })
    .await;
}

#[tokio::test]
async fn garbage_instead_of_done_and_shutdown_and_timeout() {
    for_each_backend(|b| async move {
        let (mut c, _, _) = idling(&b, Limits::default()).await;
        c.send("i1 IDLE\r\n").await;
        c.line().await;
        c.send("NOOP\r\n").await;
        let l = c.line().await;
        assert!(l.starts_with("i1 BAD"), "{l}");
        assert!(c.status("NOOP").await.contains("OK"), "the session survives and idle ended");
        // Shutdown while idling.
        c.send("i2 IDLE\r\n").await;
        c.line().await;
        c.shutdown.cancel();
        assert_eq!(c.line().await, "* BYE Server shutting down");
        // Idle timeout applies to an IDLE that nobody ends.
        let limits = Limits { idle_authenticated: Duration::from_millis(2500), ..Limits::default() };
        let mut d = Conn::open(handler_with(b.ctx().await, limits), true);
        d.line().await;
        d.login("alice@a.test", "pw").await;
        d.ok("SELECT INBOX").await;
        d.send("i1 IDLE\r\n").await;
        d.line().await;
        let l = d.line().await;
        assert!(l.starts_with("* BYE") && l.contains("idle"), "{l}");
    })
    .await;
}
