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
        let off = b.dir.create_domain("off.test").await.unwrap();
        b.dir.create_account("x", off, None, None).await.unwrap();
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<x@remote.example>").await;
        let r = c.cmd("RCPT TO:<nobody@example.net>").await;
        assert!(r.starts_with("550 5.1.1"), "unknown local user: {r}");
        b.dir.set_status(bob, AccountStatus::Disabled).await.unwrap();
        let r = c.cmd("RCPT TO:<bob@example.net>").await;
        assert!(r.starts_with("550 5.2.1"), "suspended account: {r}");
        b.dir.set_status(bob, AccountStatus::Locked).await.unwrap();
        assert!(c.cmd("RCPT TO:<bob@example.net>").await.starts_with("550 5.2.1"));
        b.dir.set_domain_status(off, rusty_directory::DomainStatus::Disabled).await.unwrap();
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
        let list = b.dir.create_list("team", dom).await.unwrap();
        b.dir.add_list_member(list, Some(alice), None).await.unwrap();
        b.dir.add_list_member(list, Some(bob), None).await.unwrap();
        b.dir.add_list_member(list, None, Some("far@away.example")).await.unwrap();
        let only_ext = b.dir.create_list("ext", dom).await.unwrap();
        b.dir.add_list_member(only_ext, None, Some("far@away.example")).await.unwrap();
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
        let big = b.dir.create_list("everyone", dom).await.unwrap();
        for i in 0..1001 {
            let a = b.dir.create_account(&format!("m{i:04}"), dom, None, None).await.unwrap();
            b.dir.add_list_member(big, Some(a), None).await.unwrap();
        }
        let s = start(&b, Opts::default()).await;
        let mut c = Client::hello(s.addr).await;
        c.cmd("MAIL FROM:<s@remote.example>").await;
        let r = c.cmd("RCPT TO:<everyone@example.net>").await;
        assert!(r.starts_with("550 5.5.3"), "a list that can never be expanded is permanent, not 452: {r}");
        s.stop().await;

        // the per-message cap: a small list is fine alone, but two recipients together exceed 3 mailboxes
        let small = b.dir.create_list("small", dom).await.unwrap();
        for who in ["m0001", "m0002"] {
            let a = b.dir.find_account(&format!("{who}@example.net")).await.unwrap().unwrap().id;
            b.dir.add_list_member(small, Some(a), None).await.unwrap();
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
        let list = b.dir.create_list("team", dom).await.unwrap();
        b.dir.add_list_member(list, Some(alice), None).await.unwrap();
        let other = b.dir.create_domain("other.test").await.unwrap();
        let eve = b.dir.create_account("eve", other, None, None).await.unwrap();
        let foreign = b.dir.create_list("friends", other).await.unwrap();
        assert!(b.dir.add_list_member(foreign, Some(alice), None).await.is_err(), "no cross-tenant list member");
        b.dir.add_list_member(foreign, Some(eve), None).await.unwrap();
        b.dir.set_domain_status(dom, rusty_directory::DomainStatus::Disabled).await.unwrap();
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
        b.dir.remove_alias("postmaster@example.net").await.unwrap();
        let r = c.mail("s@remote.example", &["postmaster@example.net"], b"Subject: two\r\n\r\nx").await;
        assert!(r.starts_with("250"), "{r}");
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 2);
        // the oldest is disabled: the next active one
        b.dir.set_status(alice, AccountStatus::Disabled).await.unwrap();
        let r = c.mail("s@remote.example", &["POSTMASTER@example.net"], b"Subject: three\r\n\r\nx").await;
        assert!(r.starts_with("250"), "{r}");
        assert_eq!(b.inbox(bob, &s.blobs).await.len(), 1);
        // nobody active at all: there is nobody to deliver to
        b.dir.set_status(bob, AccountStatus::Disabled).await.unwrap();
        c.cmd("MAIL FROM:<s@remote.example>").await;
        let r = c.cmd("RCPT TO:<postmaster@example.net>").await;
        assert!(r.starts_with("550"), "{r}");
        // a domain with no accounts at all
        b.dir.create_domain("empty.test").await.unwrap();
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
