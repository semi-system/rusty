//! End to end across the two services of one process: a message delivered by the SMTP inbound
//! handler wakes an IDLE session of the recipient, because both open mailboxes through the same
//! MailboxHub (whose ChangeNotifier announces every committed write).

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use rusty_smtp::inbound::{InboundHandler, InboundOptions};
use rusty_svc::{BoxedStream, SessionHandler};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

async fn smtp_deliver(ctx: &rusty_svc::Context, to: &str) {
    let h = Arc::new(InboundHandler::new(ctx, InboundOptions::default()));
    let (mut client, server) = tokio::io::duplex(1 << 16);
    let task = tokio::spawn(async move {
        let s: BoxedStream = Box::new(server);
        h.handle(s, peer(false), CancellationToken::new()).await;
    });
    let mut buf = vec![0u8; 4096];
    let _ = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf)).await;
    let script = format!(
        "EHLO c.example\r\nMAIL FROM:<s@remote.example>\r\nRCPT TO:<{to}>\r\nDATA\r\nFrom: s@remote.example\r\nTo: {to}\r\nSubject: pushed\r\n\r\nbody\r\n.\r\nQUIT\r\n"
    );
    client.write_all(script.as_bytes()).await.unwrap();
    // Read until the server hangs up (after QUIT).
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

#[tokio::test]
async fn an_smtp_delivery_wakes_an_idling_imap_session() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw").await;
        let ctx = b.ctx().await;
        let mut c = Conn::open(handler(ctx.clone()), true);
        c.line().await;
        c.login("alice@a.test", "pw").await;
        c.ok("SELECT INBOX").await;
        c.send("i1 IDLE\r\n").await;
        assert_eq!(c.line().await, "+ idling");
        let t0 = Instant::now();
        smtp_deliver(&ctx, "alice@a.test").await;
        assert_eq!(c.line().await, "* 1 EXISTS", "{}", b.name);
        assert!(t0.elapsed() < Duration::from_secs(5));
        c.send("DONE\r\n").await;
        assert!(c.line().await.starts_with("i1 OK"));
    })
    .await;
}
