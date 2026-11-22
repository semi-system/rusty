//! Inbound SMTP (port 25) end to end: a real TCP loopback client against the real driver over a
#![allow(clippy::field_reassign_with_default)]
//! real store, on every backend.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{for_each_backend, start, Client, Opts, HOST};
use rusty_core::config::{LogFormat, LogLevel};
use rusty_core::FolderId;
use rusty_smtp::filter::{FilterContext, FilterVerdict, InboundFilter, QuarantineFolder};
use rusty_smtp::inbound::InboundOptions;
use rusty_store::repo::AccountStatus;
use tracing_subscriber::fmt::MakeWriter;

const MSG: &[u8] = b"From: Sender <sender@remote.example>\r\nTo: alice@example.net\r\nSubject: hello\r\n\r\nbody line\r\n";

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn a_message_for_a_local_user_lands_in_the_inbox_with_trace_headers() {
    for_each_backend(|b| async move {
        let (alice, bob) = b.seed().await;
        let s = start(&b, Opts::default()).await;
        let mut c = Client::connect(s.addr).await;
        assert_eq!(c.reply().await, format!("220 {HOST} ESMTP\r\n"));
        let ehlo = c.cmd("EHLO client.example.org").await;
        assert!(!ehlo.contains("AUTH") && !ehlo.contains("STARTTLS"), "{ehlo}");
        let r = c.mail("sender@remote.example", &["alice@example.net"], MSG.strip_suffix(b"\r\n").unwrap()).await;
        assert!(r.starts_with("250 2.0.0"), "{r}");
        let _ = c.cmd("QUIT").await;
        let inbox = b.inbox(alice, &s.blobs).await;
        assert_eq!(inbox.len(), 1);
        let m = &inbox[0];
        assert!(m.starts_with(b"Return-Path: <sender@remote.example>\r\nReceived: from client.example.org ([127.0.0.1])"));
        assert!(contains(m, format!("by {HOST} with ESMTP id ").as_bytes()));
        assert!(contains(m, format!("Authentication-Results: {HOST}; none\r\n").as_bytes()));
        assert!(m.ends_with(MSG), "the original message follows the trace headers, byte-exact");
        assert!(b.inbox(bob, &s.blobs).await.is_empty());
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn relay_to_a_foreign_domain_is_refused_before_data() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        assert!(c.cmd("MAIL FROM:<x@remote.example>").await.starts_with("250"));
        let r = c.cmd("RCPT TO:<someone@elsewhere.example>").await;
        assert!(r.starts_with("554 5.7.1"), "{r}");
        let r = c.cmd("DATA").await;
        assert!(r.starts_with("554 5.5.1") || r.starts_with("503"), "DATA must not open without a valid recipient: {r}");
        assert!(b.inbox(alice, &s.blobs).await.is_empty());
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn unknown_suspended_and_disabled_domain_recipients() {
    for_each_backend(|b| async move {
        let (_, bob) = b.seed().await;
        let off = b.seed.create_domain("off.test").await.unwrap();
        b.seed.create_account("x", off, None, None).await.unwrap();
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<x@remote.example>").await;
        let r = c.cmd("RCPT TO:<nobody@example.net>").await;
        assert!(r.starts_with("550 5.1.1"), "unknown local user: {r}");
        b.seed.set_status(bob, AccountStatus::Disabled).await.unwrap();
        let r = c.cmd("RCPT TO:<bob@example.net>").await;
        assert!(r.starts_with("550 5.2.1"), "suspended account: {r}");
        // Locked: the person cannot sign in, but mail for the account is still accepted.
        b.seed.set_status(bob, AccountStatus::Locked).await.unwrap();
        let r = c.cmd("RCPT TO:<bob@example.net>").await;
        assert!(r.starts_with("250"), "a locked account still receives: {r}");
        b.seed.set_domain_status(off, rusty_directory::DomainStatus::Disabled).await.unwrap();
        let r = c.cmd("RCPT TO:<x@off.test>").await;
        assert!(r.starts_with("554 5.7.1"), "a disabled domain is not local: {r}");
        let r = c.cmd("RCPT TO:<a b@example.net>").await;
        assert!(r.starts_with("501 5.1.3") || r.starts_with("501"), "{r}");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn an_oversize_message_is_552_and_nothing_is_stored() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let mut o = Opts::default();
        o.max_message_bytes = 2048;
        let s = start(&b, o).await;
        // declared too large at MAIL
        let mut c = Client::hello(s.addr).await;
        let r = c.cmd("MAIL FROM:<x@remote.example> SIZE=999999").await;
        assert!(r.starts_with("552 5.3.4"), "{r}");
        // undeclared but too large at DATA
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<x@remote.example>").await;
        c.cmd("RCPT TO:<alice@example.net>").await;
        c.cmd("DATA").await;
        let big = vec![b'a'; 100_000];
        let mut payload = Vec::new();
        for chunk in big.chunks(70) {
            payload.extend_from_slice(chunk);
            payload.extend_from_slice(b"\r\n");
        }
        c.send(&payload).await;
        let r = c.reply().await;
        assert!(r.starts_with("552 5.3.4"), "{r}");
        assert!(c.closed_within(Duration::from_secs(5)).await, "the server closes after an oversize message");
        assert!(b.inbox(alice, &s.blobs).await.is_empty());
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn dot_stuffing_and_8bit_utf8_are_stored_byte_exact() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        let body = "Subject: =?UTF-8?B?w6k=?= caf\u{e9}\r\n\r\n..leading dot\r\nline with caf\u{e9} \u{1f600}\r\n.\r\nnot the end\r\n..\r\n".as_bytes();
        // everything up to the final CRLF.CRLF that `mail` appends; a lone "." line mid-body would end DATA,
        // so it is sent stuffed (".."): the stored text has it unstuffed
        let wire = body.to_vec();
        let wire = String::from_utf8(wire).unwrap().replace("\r\n.\r\nnot the end", "\r\n..\r\nnot the end");
        let r = c.mail("s@remote.example", &["alice@example.net"], wire.trim_end_matches("\r\n").as_bytes()).await;
        assert!(r.starts_with("250"), "{r}");
        let m = &b.inbox(alice, &s.blobs).await[0];
        let expected = "Subject: =?UTF-8?B?w6k=?= caf\u{e9}\r\n\r\n.leading dot\r\nline with caf\u{e9} \u{1f600}\r\n.\r\nnot the end\r\n.\r\n";
        assert!(m.ends_with(expected.as_bytes()), "{:?}", String::from_utf8_lossy(m));
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn two_addresses_of_one_account_store_one_message() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        // alice@ and the postmaster@ alias are the same account
        let r = c.mail("s@remote.example", &["alice@example.net", "postmaster@example.net", "ALICE@example.net"], MSG).await;
        assert!(r.starts_with("250"), "{r}");
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1);
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn list_members_each_get_one_copy_and_external_members_are_skipped() {
    for_each_backend(|b| async move {
        let (alice, bob) = b.seed().await;
        let dom = b.dir.find_domain("example.net").await.unwrap().unwrap().id;
        let list = b.seed.create_list("team", dom).await.unwrap();
        b.seed.add_list_member(list, Some(alice), None).await.unwrap();
        b.seed.add_list_member(list, Some(bob), None).await.unwrap();
        b.seed.add_list_member(list, None, Some("far@away.example")).await.unwrap();
        let only_ext = b.seed.create_list("ext", dom).await.unwrap();
        b.seed.add_list_member(only_ext, None, Some("far@away.example")).await.unwrap();
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        let r = c.mail("s@remote.example", &["team@example.net", "alice@example.net"], MSG).await;
        assert!(r.starts_with("250"), "{r}");
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1, "deduplicated with the direct recipient");
        assert_eq!(b.inbox(bob, &s.blobs).await.len(), 1);
        c.cmd("RSET").await;
        c.cmd("MAIL FROM:<s@remote.example>").await;
        let r = c.cmd("RCPT TO:<ext@example.net>").await;
        assert!(r.starts_with("550"), "a list without local members delivers nowhere: {r}");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn mail_to_a_list_with_outside_members_is_forwarded_only_for_a_member_and_a_loop_is_refused() {
    for_each_backend(|b| async move {
        let (alice, bob) = b.seed().await;
        let dom = b.dir.find_domain("example.net").await.unwrap().unwrap().id;
        let list = b.seed.create_list("team", dom).await.unwrap();
        b.seed.add_list_member(list, Some(alice), None).await.unwrap();
        b.seed.add_list_member(list, Some(bob), None).await.unwrap();
        b.seed.add_list_member(list, None, Some("far@away.example")).await.unwrap();
        let mut o = Opts::default();
        o.forward = true;
        let s = start(&b, o).await;
        // An outside member writes: every local member gets a copy and the outside member's own copy is not sent back to them... (it is queued like any other outside member).
        let mut c = Client::hello(s.addr).await;
        let r = c.mail("far@away.example", &["team@example.net"], MSG).await;
        assert!(r.starts_with("250"), "{r}");
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1);
        assert_eq!(b.inbox(bob, &s.blobs).await.len(), 1);
        assert_eq!(b.count("jobs").await, 1, "one job for the outside member");
        let kept = b.inbox(alice, &s.blobs).await;
        assert!(String::from_utf8_lossy(&kept[0]).contains("X-Rusty-List: team@example.net"));
        // A local member writes: allowed.
        let mut c = Client::hello(s.addr).await;
        assert!(c.mail("alice@example.net", &["team@example.net"], MSG).await.starts_with("250"));
        // A stranger may not use the list.
        let mut c = Client::hello(s.addr).await;
        assert!(c.cmd("MAIL FROM:<stranger@remote.example>").await.starts_with("250"));
        assert!(c.cmd("RCPT TO:<team@example.net>").await.starts_with("250"));
        assert!(c.cmd("DATA").await.starts_with("354"));
        c.send(MSG).await;
        c.send(b".\r\n").await;
        let r = c.reply().await;
        assert!(r.starts_with("550 5.7.1"), "{r}");
        // The null sender (a bounce) may not either, and a message already marked by this list is a loop.
        let mut c = Client::hello(s.addr).await;
        assert!(c.cmd("MAIL FROM:<far@away.example>").await.starts_with("250"));
        assert!(c.cmd("RCPT TO:<team@example.net>").await.starts_with("250"));
        assert!(c.cmd("DATA").await.starts_with("354"));
        c.send(b"From: far@away.example\r\nX-Rusty-List: team@example.net\r\nSubject: loop\r\n\r\nx\r\n").await;
        c.send(b".\r\n").await;
        let r = c.reply().await;
        assert!(r.starts_with("554 5.4.6"), "{r}");
        assert_eq!(b.count("jobs").await, 2, "the refused messages queued nothing");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_recipient_cap_gives_452() {
    for_each_backend(|b| async move {
        b.seed().await;
        let mut o = Opts::default();
        o.max_recipients = 2;
        let s = start(&b, o).await;
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<s@remote.example>").await;
        assert!(c.cmd("RCPT TO:<alice@example.net>").await.starts_with("250"));
        assert!(c.cmd("RCPT TO:<bob@example.net>").await.starts_with("250"));
        assert!(c.cmd("RCPT TO:<postmaster@example.net>").await.starts_with("452 4.5.3"));
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_retry_of_the_same_message_is_stored_once() {
    // The remote MTA never saw the 250 (crash between commit and reply) and sends it again on a new connection.
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let s = start(&b, Opts::default()).await;
        for _ in 0..2 {
            let mut c = Client::hello(s.addr).await;
            let r = c.mail("sender@remote.example", &["alice@example.net"], MSG).await;
            assert!(r.starts_with("250"), "{r}");
            c.cmd("QUIT").await;
        }
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1);
        // a different sender with the same bytes is a different delivery
        let mut c = Client::hello(s.addr).await;
        c.mail("other@remote.example", &["alice@example.net"], MSG).await;
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 2);
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn forged_authentication_results_and_trace_headers_are_stripped() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        let forged = format!(
            "Authentication-Results: {HOST}; dkim=pass\r\nReceived-SPF: pass\r\nX-Rusty-Verified: yes\r\nSubject: s\r\n\r\nbody\r\n"
        );
        let r = c.mail("s@remote.example", &["alice@example.net"], forged.trim_end_matches("\r\n").as_bytes()).await;
        assert!(r.starts_with("250"), "{r}");
        let m = String::from_utf8(b.inbox(alice, &s.blobs).await.remove(0)).unwrap();
        assert!(!m.contains("dkim=pass") && !m.contains("Received-SPF") && !m.to_lowercase().contains("x-rusty"), "{m}");
        assert_eq!(m.matches("Authentication-Results:").count(), 1, "{m}");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn starttls_upgrades_resets_state_and_marks_the_message() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let mut o = Opts::default();
        o.tls = true;
        let s = start(&b, o).await;
        let mut c = Client::connect(s.addr).await;
        c.reply().await;
        let ehlo = c.cmd("EHLO client.example.org").await;
        assert!(ehlo.contains("STARTTLS"), "{ehlo}");
        c.cmd("MAIL FROM:<s@remote.example>").await; // state that STARTTLS must discard
        let r = c.cmd("STARTTLS").await;
        assert!(r.starts_with("220 2.0.0"), "{r}");
        let mut c = c.upgrade_tls(&s.cert_der).await;
        // the old transaction and the greeting are gone
        let r = c.cmd("MAIL FROM:<s@remote.example>").await;
        assert!(r.starts_with("503 5.5.1"), "{r}");
        let ehlo = c.cmd("EHLO client.example.org").await;
        assert!(ehlo.starts_with("250") && !ehlo.contains("STARTTLS"), "{ehlo}");
        let r = c.mail("s@remote.example", &["alice@example.net"], MSG).await;
        assert!(r.starts_with("250"), "{r}");
        let m = b.inbox(alice, &s.blobs).await.remove(0);
        assert!(contains(&m, b"with ESMTPS"), "{:?}", String::from_utf8_lossy(&m));
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn plaintext_injected_behind_starttls_is_never_processed() {
    for_each_backend(|b| async move {
        b.seed().await;
        let mut o = Opts::default();
        o.tls = true;
        let s = start(&b, o).await;
        let mut c = Client::hello(s.addr).await;
        c.send(b"STARTTLS\r\nMAIL FROM:<evil@x.y>\r\n").await;
        let r = c.reply().await;
        assert!(r.starts_with("554") || r.starts_with("503"), "{r}");
        assert!(c.closed_within(Duration::from_secs(5)).await);
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_silent_client_is_cut_at_the_idle_limit() {
    for_each_backend(|b| async move {
        b.seed().await;
        let mut o = Opts::default();
        o.options = InboundOptions { command_idle: Duration::from_millis(300), data_idle: Duration::from_millis(300), ..InboundOptions::default() };
        let s = start(&b, o).await;
        let mut c = Client::connect(s.addr).await;
        c.reply().await;
        let t0 = std::time::Instant::now();
        let r = c.reply().await; // nothing sent: the server speaks next, with 421
        assert!(r.starts_with("421 4.4.2"), "{r}");
        assert!(t0.elapsed() < Duration::from_secs(3));
        assert!(c.closed_within(Duration::from_secs(3)).await);
        // a client that stalls in the middle of DATA is cut too
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<s@remote.example>").await;
        c.cmd("RCPT TO:<alice@example.net>").await;
        c.cmd("DATA").await;
        c.send(b"Subject: slow\r\n").await;
        let r = c.reply().await;
        assert!(r.starts_with("421 4.4.2"), "{r}");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn nothing_is_processed_after_quit() {
    for_each_backend(|b| async move {
        b.seed().await;
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        c.send(b"QUIT\r\nNOOP\r\n").await;
        let r = c.reply().await;
        assert!(r.starts_with("221 2.0.0"), "{r}");
        assert!(c.closed_within(Duration::from_secs(5)).await);
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn shutdown_says_421_to_an_idle_session() {
    for_each_backend(|b| async move {
        b.seed().await;
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        s.token.cancel();
        let r = c.reply().await;
        assert!(r.starts_with("421 4.3.2"), "{r}");
        s.stop().await;
    })
    .await;
}

struct Reject;

#[async_trait]
impl InboundFilter for Reject {
    async fn check(&self, _: &FilterContext<'_>) -> FilterVerdict {
        FilterVerdict::Reject { code: 554, enhanced: "5.7.1", text: "Message refused by policy".into() }
    }
}

struct Tag;

#[async_trait]
impl InboundFilter for Tag {
    async fn check(&self, _: &FilterContext<'_>) -> FilterVerdict {
        FilterVerdict::Tag(vec![("X-Test-Verdict".into(), "tagged".into())])
    }
}

struct ToSpam;

#[async_trait]
impl InboundFilter for ToSpam {
    async fn check(&self, _: &FilterContext<'_>) -> FilterVerdict {
        FilterVerdict::Quarantine(QuarantineFolder::Spam)
    }
}

#[tokio::test]
async fn the_inbound_filter_pipeline_can_reject_tag_and_quarantine() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        // reject: refused at the end of DATA, nothing stored
        let mut o = Opts::default();
        o.options.filters = vec![Arc::new(Reject)];
        let s = start(&b, o).await;
        let mut c = Client::hello(s.addr).await;
        let r = c.mail("s@remote.example", &["alice@example.net"], MSG).await;
        assert!(r.starts_with("554 5.7.1"), "{r}");
        assert!(b.inbox(alice, &s.blobs).await.is_empty());
        s.stop().await;
        // tag + quarantine
        let mut o = Opts::default();
        o.options.filters = vec![Arc::new(Tag), Arc::new(ToSpam)];
        let s = start(&b, o).await;
        let mut c = Client::hello(s.addr).await;
        let r = c.mail("s@remote.example", &["alice@example.net"], MSG).await;
        assert!(r.starts_with("250"), "{r}");
        assert!(b.inbox(alice, &s.blobs).await.is_empty());
        let spam = b.folder(alice, FolderId(5), &s.blobs).await;
        assert_eq!(spam.len(), 1);
        assert!(contains(&spam[0], b"X-Test-Verdict: tagged\r\n"));
        s.stop().await;
    })
    .await;
}

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf {
        self.clone()
    }
}

#[tokio::test]
async fn message_bodies_and_auth_blobs_never_reach_the_log() {
    // current-thread runtime: the server task logs on this thread, where the capture is installed
    for_each_backend(|b| async move {
        b.seed().await;
        let buf = Buf::default();
        let sub = rusty_svc::subscriber(LogLevel::Trace, LogFormat::Text, buf.clone());
        let _guard = tracing::subscriber::set_default(sub);
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        c.cmd("AUTH PLAIN c2VjcmV0LWJsb2ItMTIz").await;
        c.cmd("FROBNICATE secret-command-token").await;
        c.mail("s@remote.example", &["alice@example.net"], b"Subject: s\r\n\r\nSECRET-BODY-TOKEN-777").await;
        c.cmd("QUIT").await;
        s.stop().await;
        let log = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        for token in ["SECRET-BODY-TOKEN-777", "c2VjcmV0LWJsb2ItMTIz", "secret-command-token"] {
            assert!(!log.contains(token), "{token} reached the log:\n{log}");
        }
    })
    .await;
}

/// A clock that moves one second per reading (a retry that crosses a second boundary).
struct Ticking(std::sync::atomic::AtomicI64);

impl rusty_smtp::inbound::WallClock for Ticking {
    fn now_unix(&self) -> i64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }
}

#[tokio::test]
async fn a_retry_whose_trace_headers_differ_is_still_stored_once() {
    // The Received header carries a time, so the stored bytes of a retry differ from the first copy; the
    // delivery id (derived from the original data) must still make it a duplicate, not a failure.
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let mut o = Opts::default();
        o.options.clock = Arc::new(Ticking(std::sync::atomic::AtomicI64::new(1_794_139_200)));
        let s = start(&b, o).await;
        for _ in 0..3 {
            let mut c = Client::hello(s.addr).await;
            let r = c.mail("sender@remote.example", &["alice@example.net"], MSG).await;
            assert!(r.starts_with("250"), "{r}");
            c.cmd("QUIT").await;
        }
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1);
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_client_that_drips_bytes_cannot_hold_the_connection() {
    // review I6: the idle timer used to restart on every byte, so one byte per 200 ms lived forever
    for_each_backend(|b| async move {
        b.seed().await;
        let mut o = Opts::default();
        o.options = InboundOptions { command_idle: Duration::from_millis(400), data_idle: Duration::from_millis(400), ..InboundOptions::default() };
        let s = start(&b, o).await;
        // a command line that never ends, one byte per 150 ms
        let mut c = Client::connect(s.addr).await;
        c.reply().await;
        let t0 = std::time::Instant::now();
        let mut cut = false;
        for _ in 0..30 {
            if !c.try_send(b"N").await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
            if t0.elapsed() > Duration::from_millis(1500) {
                break;
            }
        }
        let r = c.reply().await;
        assert!(r.starts_with("421 4.4.2"), "{r}");
        cut = cut || t0.elapsed() < Duration::from_secs(3);
        assert!(cut, "the drip must be cut within the budget, took {:?}", t0.elapsed());
        // DATA at a trickle: after the grace the average rate is far below the minimum
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<s@remote.example>").await;
        c.cmd("RCPT TO:<alice@example.net>").await;
        c.cmd("DATA").await;
        let t0 = std::time::Instant::now();
        for _ in 0..40 {
            if !c.try_send(b"x").await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            if t0.elapsed() > Duration::from_secs(4) {
                break;
            }
        }
        let r = c.reply().await;
        assert!(r.starts_with("421 4.4.2"), "{r}");
        assert!(t0.elapsed() < Duration::from_secs(6), "{:?}", t0.elapsed());
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_list_over_the_limit_is_a_permanent_refusal_and_a_big_message_fan_out_is_bounded() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let dom = b.dir.find_domain("example.net").await.unwrap().unwrap().id;
        let big = b.seed.create_list("everyone", dom).await.unwrap();
        for i in 0..1001 {
            let a = b.seed.create_account(&format!("m{i:04}"), dom, None, None).await.unwrap();
            b.seed.add_list_member(big, Some(a), None).await.unwrap();
        }
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<s@remote.example>").await;
        let r = c.cmd("RCPT TO:<everyone@example.net>").await;
        assert!(r.starts_with("550 5.5.3"), "a list that can never be expanded is permanent, not 452: {r}");
        s.stop().await;

        // the per-message cap: a small list is fine alone, but two recipients together exceed 3 mailboxes
        let small = b.seed.create_list("small", dom).await.unwrap();
        for who in ["m0001", "m0002"] {
            let a = b.dir.find_account(&format!("{who}@example.net")).await.unwrap().unwrap().id;
            b.seed.add_list_member(small, Some(a), None).await.unwrap();
        }
        let mut o = Opts::default();
        o.options.max_accounts = 3;
        let s = start(&b, o).await;
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<s@remote.example>").await;
        assert!(c.cmd("RCPT TO:<small@example.net>").await.starts_with("250"));
        assert!(c.cmd("RCPT TO:<alice@example.net>").await.starts_with("250"));
        let r = c.cmd("RCPT TO:<bob@example.net>").await;
        assert!(r.starts_with("452 4.5.3"), "{r}");
        let _ = alice;
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_disabled_domains_accounts_get_no_mail_even_through_a_list() {
    // review I8: lists are same-tenant (I4), so a disabled domain's accounts cannot be members of another
    // tenant's list; inside the domain the whole domain is off and the list resolves nothing
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let dom = b.dir.find_domain("example.net").await.unwrap().unwrap().id;
        let list = b.seed.create_list("team", dom).await.unwrap();
        b.seed.add_list_member(list, Some(alice), None).await.unwrap();
        let other = b.seed.create_domain("other.test").await.unwrap();
        let eve = b.seed.create_account("eve", other, None, None).await.unwrap();
        let foreign = b.seed.create_list("friends", other).await.unwrap();
        assert!(b.seed.add_list_member(foreign, Some(alice), None).await.is_err(), "no cross-tenant list member");
        b.seed.add_list_member(foreign, Some(eve), None).await.unwrap();
        b.seed.set_domain_status(dom, rusty_directory::DomainStatus::Disabled).await.unwrap();
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<s@remote.example>").await;
        assert!(c.cmd("RCPT TO:<alice@example.net>").await.starts_with("554 5.7.1"));
        assert!(c.cmd("RCPT TO:<team@example.net>").await.starts_with("554 5.7.1"));
        assert!(c.cmd("RCPT TO:<friends@other.test>").await.starts_with("250"), "the other tenant is unaffected");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn two_recipients_get_their_own_delivery_ids_and_a_resend_to_a_third_only_adds_that_one() {
    // the account is part of the delivery id: the same bytes for a new recipient are a new delivery
    for_each_backend(|b| async move {
        let (alice, bob) = b.seed().await;
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        assert!(c.mail("sender@remote.example", &["alice@example.net"], MSG).await.starts_with("250"));
        assert!(c.mail("sender@remote.example", &["alice@example.net", "bob@example.net"], MSG).await.starts_with("250"));
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1, "alice already had it");
        assert_eq!(b.inbox(bob, &s.blobs).await.len(), 1, "bob gets his own copy");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_memory_budget_refuses_data_and_deliveries_it_cannot_cover() {
    // review I5: no global memory budget; a message in flight was held about four times with no limit
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        // 64 KiB in total: a 20 KiB message is received (20 KiB) but its three extra copies (60 KiB) do not fit
        let mut o = Opts::default();
        o.options.budget = rusty_smtp::limits::MemoryBudget::new_mib(0);
        let s = start(&b, o).await;
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<s@remote.example>").await;
        c.cmd("RCPT TO:<alice@example.net>").await;
        c.cmd("DATA").await;
        c.send(b"Subject: x\r\n\r\nsome data\r\n").await;
        let r = c.reply().await;
        assert!(r.starts_with("421 4.3.2"), "no budget at all: refuse the data, tell the peer to come back: {r}");
        s.stop().await;

        // a budget that covers reception but not the extra copies: the message is refused at the end, temporarily
        let mut o = Opts::default();
        o.options.budget = rusty_smtp::limits::MemoryBudget::new_mib(1);
        let s = start(&b, o).await;
        let mut c = Client::hello(s.addr).await;
        let body = format!("Subject: big\r\n\r\n{}\r\n", "x".repeat(400_000));
        let r = c.mail("s@remote.example", &["alice@example.net"], body.as_bytes()).await;
        assert!(r.starts_with("421 4.3.2") || r.starts_with("451 4.3.1"), "{r}");
        assert!(b.inbox(alice, &s.blobs).await.is_empty());
        // the budget is returned: a small message goes through afterwards
        let mut c = Client::hello(s.addr).await;
        let r = c.mail("s@remote.example", &["alice@example.net"], b"Subject: small\r\n\r\nx").await;
        assert!(r.starts_with("250"), "{r}");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn postmaster_is_always_deliverable_where_there_is_somebody_to_give_it_to() {
    for_each_backend(|b| async move {
        let (alice, bob) = b.seed().await; // seed: postmaster@ alias -> alice; host name mx.example.net
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        // RFC 5321 4.5.1: the bare form, resolved against the own domain of the server (mx.example.net -> example.net)
        let r = c.mail("s@remote.example", &["postmaster"], MSG).await;
        assert!(r.starts_with("250"), "{r}");
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1);
        // a domain without the alias: the oldest active account receives it
        b.seed.remove_alias("postmaster@example.net").await.unwrap();
        let r = c.mail("s@remote.example", &["postmaster@example.net"], b"Subject: two\r\n\r\nx").await;
        assert!(r.starts_with("250"), "{r}");
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 2);
        // the oldest is disabled: the next active one
        b.seed.set_status(alice, AccountStatus::Disabled).await.unwrap();
        let r = c.mail("s@remote.example", &["POSTMASTER@example.net"], b"Subject: three\r\n\r\nx").await;
        assert!(r.starts_with("250"), "{r}");
        assert_eq!(b.inbox(bob, &s.blobs).await.len(), 1);
        // nobody active at all: there is nobody to deliver to
        b.seed.set_status(bob, AccountStatus::Disabled).await.unwrap();
        c.cmd("MAIL FROM:<s@remote.example>").await;
        let r = c.cmd("RCPT TO:<postmaster@example.net>").await;
        assert!(r.starts_with("550"), "{r}");
        // a domain with no accounts at all
        b.seed.create_domain("empty.test").await.unwrap();
        let r = c.cmd("RCPT TO:<postmaster@empty.test>").await;
        assert!(r.starts_with("550 5.1.1"), "{r}");
        s.stop().await;
    })
    .await;
}

/// Wait (bounded) until the shared budget is whole again: a session task releases its permits when it ends.
async fn assert_budget_whole(budget: &rusty_smtp::limits::MemoryBudget, full: usize, what: &str) {
    for _ in 0..100 {
        if budget.available_kib() == full {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what}: the budget leaked: {} of {full} KiB free", budget.available_kib());
}

#[tokio::test]
async fn the_memory_budget_is_returned_on_every_exit_path() {
    // Review U4 I7: a leaked reservation survived every test (mutants M4/M4b).
    for_each_backend(|b| async move {
        b.seed().await;
        let budget = rusty_smtp::limits::MemoryBudget::new_mib(8);
        let full = budget.available_kib();
        let mut o = Opts::default();
        o.options.budget = budget.clone();
        let s = start(&b, o).await;

        // 1. a delivery that succeeds
        let mut c = Client::hello(s.addr).await;
        let r = c.mail("s@remote.example", &["alice@example.net"], MSG.strip_suffix(b"\r\n").unwrap()).await;
        assert!(r.starts_with("250"), "{r}");
        let _ = c.cmd("QUIT").await;
        assert_budget_whole(&budget, full, "after a delivery").await;

        // 2. a message refused after it was received (too big for the server's size limit)
        let mut c = Client::hello(s.addr).await;
        let huge = format!("Subject: big\r\n\r\n{}\r\n", "x".repeat(1_300_000));
        let r = c.mail("s@remote.example", &["alice@example.net"], huge.as_bytes()).await;
        assert!(r.starts_with("552"), "{r}");
        drop(c);
        assert_budget_whole(&budget, full, "after a refused message").await;

        // 3. the client vanishes in the middle of DATA
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<s@remote.example>").await;
        c.cmd("RCPT TO:<alice@example.net>").await;
        c.cmd("DATA").await;
        c.send(&vec![b'y'; 300_000]).await;
        drop(c);
        assert_budget_whole(&budget, full, "after an early close").await;

        // 4. the server is told to stop in the middle of DATA
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<s@remote.example>").await;
        c.cmd("RCPT TO:<alice@example.net>").await;
        c.cmd("DATA").await;
        c.send(&vec![b'z'; 300_000]).await;
        s.token.cancel();
        assert_budget_whole(&budget, full, "after a shutdown").await;
        drop(c);
        s.stop().await;
    })
    .await;
}

/// U5: the `none` stub of U3 is replaced by the real SPF/DKIM/DMARC results (log-only: the message is
/// delivered whatever they say).
#[tokio::test]
async fn authentication_results_carry_the_real_spf_dkim_and_dmarc_verdicts() {
    use rusty_smtp::resolve::StaticDns;
    use rusty_smtp::stack::DnsStack;
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let mut dns = StaticDns::default();
        dns.txt.insert("sender.example".into(), "v=spf1 ip4:127.0.0.1 -all".into());
        dns.txt.insert("_dmarc.sender.example".into(), "v=DMARC1; p=reject".into());
        let stack = DnsStack::with_static(dns, HOST).unwrap();
        let mut opts = Opts::default();
        opts.options.auth = Some(stack.checker.clone());
        let s = start(&b, opts).await;

        // From the address the SPF record authorises.
        let mut c = Client::hello(s.addr).await;
        let msg = b"From: x@sender.example\r\nTo: alice@example.net\r\nSubject: spf\r\n\r\nbody";
        let r = c.mail("x@sender.example", &["alice@example.net"], msg).await;
        assert!(r.starts_with("250"), "{r}");
        let _ = c.cmd("QUIT").await;
        // A forged Authentication-Results from the sender is replaced, a mismatching identity is
        // reported (and still delivered: log-only).
        let mut c = Client::hello(s.addr).await;
        let forged = b"Authentication-Results: mx.example.net; spf=pass dkim=pass dmarc=pass\r\nFrom: y@other.example\r\nTo: alice@example.net\r\nSubject: forged\r\n\r\nbody";
        let r = c.mail("y@other.example", &["alice@example.net"], forged).await;
        assert!(r.starts_with("250"), "{r}");
        let _ = c.cmd("QUIT").await;

        let inbox = b.inbox(alice, &s.blobs).await;
        assert_eq!(inbox.len(), 2);
        let first = String::from_utf8_lossy(&inbox[0]).replace("\r\n\t", " ").replace("\r\n ", " ");
        let line = first.lines().find(|l| l.starts_with("Authentication-Results:")).unwrap().to_owned();
        assert!(line.contains(&format!("{HOST};")) && line.contains("spf=pass") && line.contains("dmarc=pass"), "{line}");
        let second = String::from_utf8_lossy(&inbox[1]).replace("\r\n\t", " ").replace("\r\n ", " ");
        assert_eq!(second.matches("Authentication-Results:").count(), 1, "the forged one is gone: {second}");
        let line2 = second.lines().find(|l| l.starts_with("Authentication-Results:")).unwrap();
        assert!(!line2.contains("dkim=pass") && !line2.contains("dmarc=pass"), "{line2}");
        s.stop().await;
    })
    .await;
}

/// A clamd that answers FOUND for a message that holds "EICAR" and OK for any other (the INSTREAM framing is read to its end).
async fn fake_clamd() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            tokio::spawn(async move {
                let mut cmd = [0u8; 10];
                if s.read_exact(&mut cmd).await.is_err() {
                    return;
                }
                let mut body = Vec::new();
                loop {
                    let mut len = [0u8; 4];
                    if s.read_exact(&mut len).await.is_err() {
                        return;
                    }
                    let n = u32::from_be_bytes(len) as usize;
                    if n == 0 {
                        break;
                    }
                    let mut chunk = vec![0u8; n];
                    if s.read_exact(&mut chunk).await.is_err() {
                        return;
                    }
                    body.extend_from_slice(&chunk);
                }
                let _ = s.write_all(if contains(&body, b"EICAR") { b"stream: Test FOUND\0" as &[u8] } else { b"stream: OK\0" }).await;
            });
        }
    });
    addr
}

/// An rspamd that scores a message that holds "SPAMMY" as `add header` and any other as `no action`.
async fn fake_rspamd() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let Ok(n) = s.read(&mut tmp).await else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).into_owned();
                        let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                        if buf.len() >= p + 4 + len {
                            break;
                        }
                    }
                }
                let json = if contains(&buf, b"SPAMMY") { r#"{"action":"add header","score":12.0,"required_score":6.0}"# } else { r#"{"action":"no action","score":0.5,"required_score":6.0}"# };
                let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}", json.len()).as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn the_real_scanners_refuse_a_virus_file_spam_in_the_spam_folder_and_mark_a_clean_message() {
    use rusty_core::config::OnError;
    use rusty_smtp::scan::{ClamdFilter, RspamdFilter};
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let (clamd, rspamd) = (fake_clamd().await, fake_rspamd().await);
        let mut o = Opts::default();
        o.options.filters = vec![
            Arc::new(ClamdFilter::new(&clamd, Duration::from_secs(5), 1024 * 1024, OnError::Tempfail)),
            Arc::new(RspamdFilter::new(&rspamd, Duration::from_secs(5), 1024 * 1024, OnError::Accept)),
        ];
        let s = start(&b, o).await;
        let mut c = Client::hello(s.addr).await;
        // A virus: refused at the end of DATA with 550, nothing stored.
        let r = c.mail("s@remote.example", &["alice@example.net"], b"From: s@remote.example\r\nTo: alice@example.net\r\nSubject: v\r\n\r\nEICAR test\r\n").await;
        assert!(r.starts_with("550 5.7.1"), "{r}");
        // Spam: delivered, into the spam folder, with the verdict in the headers.
        let mut c = Client::hello(s.addr).await;
        let r = c.mail("s@remote.example", &["alice@example.net"], b"From: s@remote.example\r\nTo: alice@example.net\r\nSubject: s\r\n\r\nSPAMMY offer\r\n").await;
        assert!(r.starts_with("250"), "{r}");
        // A clean message: in the Inbox, marked by both scanners.
        let mut c = Client::hello(s.addr).await;
        let r = c.mail("s@remote.example", &["alice@example.net"], MSG).await;
        assert!(r.starts_with("250"), "{r}");
        let inbox = b.inbox(alice, &s.blobs).await;
        assert_eq!(inbox.len(), 1, "only the clean message is in the Inbox");
        assert!(contains(&inbox[0], b"X-Virus-Scan: clean\r\n") && contains(&inbox[0], b"X-Spam-Status: No, score=0.50"), "{}", String::from_utf8_lossy(&inbox[0]));
        let spam = b.folder(alice, FolderId(5), &s.blobs).await;
        assert_eq!(spam.len(), 1);
        assert!(contains(&spam[0], b"X-Spam-Status: Yes, score=12.00") && contains(&spam[0], b"X-Virus-Scan: clean\r\n"));
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_delivery_that_takes_the_mailbox_past_90_percent_of_its_quota_leaves_one_warning() {
    for_each_backend(|b| async move {
        let (alice, bob) = b.seed().await;
        let s = start(&b, Opts::default()).await;
        // The size of a stored message is what the mailbox holds: measure it in bob's (no limit), then give alice a quota that one such message takes to about 95 %.
        let mut c = Client::hello(s.addr).await;
        assert!(c.mail("sender@remote.example", &["bob@example.net"], MSG).await.starts_with("250"));
        let size = b.inbox(bob, &s.blobs).await[0].len() as u64;
        let quota = size * 100 / 95;
        s.ctx.directory.set_account_config(alice, serde_json::json!({ "quota_bytes": quota })).await.unwrap();
        let mut c = Client::hello(s.addr).await;
        assert!(c.mail("sender@remote.example", &["alice@example.net"], MSG).await.starts_with("250"));
        let inbox = b.inbox(alice, &s.blobs).await;
        assert!(inbox.iter().any(|m| String::from_utf8_lossy(m).contains("Your mailbox is")), "the warning is in the Inbox: {}", inbox.len());
        // A second message does not add another warning.
        let mut c = Client::hello(s.addr).await;
        let _ = c.mail("other@remote.example", &["alice@example.net"], b"From: o@remote.example\r\nSubject: s\r\n\r\nb\r\n").await;
        let warnings = b.inbox(alice, &s.blobs).await.iter().filter(|m| String::from_utf8_lossy(m).contains("Your mailbox is")).count();
        assert_eq!(warnings, 1, "once");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_message_stored_for_a_local_recipient_is_in_the_delivery_log_as_received() {
    for_each_backend(|b| async move {
        let (_alice, _) = b.seed().await;
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        assert!(c.mail("sender@remote.example", &["alice@example.net"], MSG).await.starts_with("250"));
        assert_eq!(b.count("mail_log").await, 1, "one line for the one recipient");
        let rows = s.ctx.queue.trace(b.dir.find_domain("example.net").await.unwrap().unwrap().id, None, None, 10).await.unwrap();
        assert_eq!((rows[0].direction.as_str(), rows[0].event.as_str(), rows[0].recipient.as_str(), rows[0].sender.as_str()), ("in", "delivered", "alice@example.net", "sender@remote.example"));
        assert_eq!(rows[0].remote.as_deref(), Some("127.0.0.1"));
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_sender_lists_of_the_recipients_refuse_a_blocked_sender_and_let_an_allowed_one_skip_the_scanners() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let mut o = Opts::default();
        // The policy first, then a filter that would refuse everything (the scanner's place).
        o.options.filters = vec![Arc::new(rusty_smtp::policy::SenderPolicy::new(b.dir.clone())), Arc::new(Reject)];
        let s = start(&b, o).await;
        // No list: the scanner (here the refusing filter) decides.
        let mut c = Client::hello(s.addr).await;
        let r = c.mail("friend@remote.example", &["alice@example.net"], MSG).await;
        assert!(r.starts_with("554 5.7.1"), "{r}");
        // Allowed: the scanners are skipped and the mail arrives.
        s.ctx.directory.set_account_config(alice, serde_json::json!({ "sender_allow": ["friend@remote.example"], "sender_block": ["remote.example"] })).await.unwrap();
        let mut c = Client::hello(s.addr).await;
        let r = c.mail("Friend@Remote.example", &["alice@example.net"], MSG).await;
        assert!(r.starts_with("250"), "{r}");
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1);
        // Blocked: refused with the policy's own words, before any scanner.
        let mut c = Client::hello(s.addr).await;
        let r = c.mail("spam@mail.remote.example", &["alice@example.net"], MSG).await;
        assert!(r.starts_with("550 5.7.1 Sender blocked"), "{r}");
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1, "nothing more was stored");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_rules_of_an_account_forward_its_mail_and_answer_each_sender_once() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let mut o = Opts::default();
        o.forward = true;
        let s = start(&b, o).await;
        s.ctx
            .directory
            .set_account_config(alice, serde_json::json!({ "mail_rules": { "forward": { "to": ["me@other.example"], "keep": true }, "vacation": { "enabled": true, "subject": "Away", "body": "Back on Monday." } } }))
            .await
            .unwrap();
        // First message: kept, forwarded (one job) and answered (one job).
        let mut c = Client::hello(s.addr).await;
        assert!(c.mail("sender@remote.example", &["alice@example.net"], MSG).await.starts_with("250"));
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1, "the copy is kept");
        assert_eq!(b.count("jobs").await, 2, "a forward and an automatic reply");
        // The same sender again: forwarded, but not answered a second time.
        let mut c = Client::hello(s.addr).await;
        assert!(c.mail("sender@remote.example", &["alice@example.net"], &[MSG, b"A second message.
"].concat()).await.starts_with("250"));
        assert_eq!(b.count("jobs").await, 3, "only the forward");
        // A list's mail is never answered; the null sender neither.
        let bulk = b"From: news@remote.example\r\nTo: alice@example.net\r\nSubject: News\r\nList-Id: <news.remote.example>\r\n\r\nHi\r\n";
        let mut c = Client::hello(s.addr).await;
        assert!(c.mail("news@remote.example", &["alice@example.net"], bulk).await.starts_with("250"));
        assert_eq!(b.count("jobs").await, 4, "forwarded, not answered");
        s.stop().await;
    })
    .await;
}
