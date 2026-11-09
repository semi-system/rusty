#![allow(clippy::field_reassign_with_default)]
//! Submission (587 STARTTLS, 465 implicit TLS) end to end over real TCP and TLS, on every backend.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{b64, for_each_backend, plain, start, Backend, Client, Opts};
use rusty_core::config::{LogFormat, LogLevel};
use rusty_core::AccountId;
use rusty_store::repo::AccountStatus;
use tracing_subscriber::fmt::MakeWriter;

const ALICE_PW: &str = "pw-alice-1234";

fn opts() -> Opts {
    let mut o = Opts::default();
    o.tls = true;
    o.submission = true;
    o.options.max_transactions = 100;
    o
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// alice (with alias ally@), bob, and a second tenant with eve.
async fn world(b: &Backend) -> (AccountId, AccountId, AccountId) {
    let (alice, bob) = b.seed().await;
    let dom = b.dir.find_domain("example.net").await.unwrap().unwrap().id;
    b.dir.add_alias("ally", dom, alice).await.unwrap();
    let other = b.dir.create_domain("other.test").await.unwrap();
    let eve = b.dir.create_account("eve", other, Some("pw-eve-123456"), None).await.unwrap();
    (alice, bob, eve)
}

async fn authed(addr: std::net::SocketAddr, cert: &[u8]) -> Client {
    let mut c = Client::secure_hello(addr, cert).await;
    let r = c.cmd(&format!("AUTH PLAIN {}", plain("alice@example.net", ALICE_PW))).await;
    assert!(r.starts_with("235 2.7.0"), "{r}");
    c
}

const MSG: &[u8] = b"From: Alice <alice@example.net>\r\nTo: bob@example.net\r\nSubject: hi\r\n\r\nbody\r\n";

#[tokio::test]
async fn auth_is_neither_offered_nor_accepted_before_starttls() {
    for_each_backend(|b| async move {
        world(&b).await;
        let s = start(&b, opts()).await;
        let mut c = Client::connect(s.addr).await;
        c.reply().await;
        let ehlo = c.cmd("EHLO client.example.org").await;
        assert!(ehlo.contains("STARTTLS") && !ehlo.contains("AUTH"), "{ehlo}");
        let r = c.cmd(&format!("AUTH PLAIN {}", plain("alice@example.net", ALICE_PW))).await;
        assert!(r.starts_with("538 5.7.11"), "{r}");
        let r = c.cmd("AUTH LOGIN").await;
        assert!(r.starts_with("538 5.7.11"), "{r}");
        let r = c.cmd("MAIL FROM:<alice@example.net>").await;
        assert!(r.starts_with("530 5.7.0"), "{r}");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_user_submits_to_a_local_recipient_over_starttls() {
    for_each_backend(|b| async move {
        let (alice, bob, _) = world(&b).await;
        let s = start(&b, opts()).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        let r = c.mail("alice@example.net", &["bob@example.net"], MSG.strip_suffix(b"\r\n").unwrap()).await;
        assert!(r.starts_with("250"), "{r}");
        let inbox = b.inbox(bob, &s.blobs).await;
        assert_eq!(inbox.len(), 1);
        let m = &inbox[0];
        assert!(contains(m, b"with ESMTPSA id "), "{:?}", String::from_utf8_lossy(m));
        assert!(!contains(m, b"127.0.0.1"), "the submitting client's address must not be published");
        assert!(contains(m, b"\r\nMessage-ID: <"), "a missing Message-ID is added");
        assert!(contains(m, b"\r\nDate: "), "a missing Date is added");
        assert!(m.ends_with(MSG) || contains(m, b"From: Alice <alice@example.net>"));
        assert!(b.inbox(alice, &s.blobs).await.is_empty(), "the server writes no Sent copy");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn auth_login_works_and_existing_message_id_and_date_are_kept() {
    for_each_backend(|b| async move {
        let (_, bob, _) = world(&b).await;
        let s = start(&b, opts()).await;
        let mut c = Client::secure_hello(s.addr, &s.cert_der).await;
        assert_eq!(c.cmd("AUTH LOGIN").await, "334 VXNlcm5hbWU6\r\n");
        assert_eq!(c.cmd(&b64(b"alice@example.net")).await, "334 UGFzc3dvcmQ6\r\n");
        assert!(c.cmd(&b64(ALICE_PW.as_bytes())).await.starts_with("235"));
        let msg = b"From: alice@example.net\r\nMessage-ID: <mine@client>\r\nDate: Mon, 1 Jan 2024 00:00:00 +0000\r\n\r\nx";
        assert!(c.mail("alice@example.net", &["bob@example.net"], msg).await.starts_with("250"));
        let m = b.inbox(bob, &s.blobs).await.remove(0);
        assert_eq!(m.windows(11).filter(|w| *w == b"Message-ID:").count(), 1);
        assert!(contains(&m, b"Message-ID: <mine@client>") && contains(&m, b"Date: Mon, 1 Jan 2024"));
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn wrong_passwords_are_535_and_count_toward_the_throttle() {
    for_each_backend(|b| async move {
        world(&b).await;
        let mut o = opts();
        o.auth_failures_max = 2;
        let s = start(&b, o).await;
        for _ in 0..2 {
            let mut c = Client::secure_hello(s.addr, &s.cert_der).await;
            let r = c.cmd(&format!("AUTH PLAIN {}", plain("alice@example.net", "wrong-password"))).await;
            assert!(r.starts_with("535 5.7.8"), "{r}");
        }
        // the identity is now throttled for this peer: even the right password is refused for a while
        let mut c = Client::secure_hello(s.addr, &s.cert_der).await;
        let r = c.cmd(&format!("AUTH PLAIN {}", plain("alice@example.net", ALICE_PW))).await;
        assert!(r.starts_with("454 4.7.0"), "{r}");
        // another identity from the same peer is not affected
        let r = c.cmd(&format!("AUTH PLAIN {}", plain("bob@example.net", "pw-bob-12345"))).await;
        assert!(r.starts_with("235"), "{r}");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn disabled_accounts_and_unknown_users_cannot_log_in_and_look_alike() {
    for_each_backend(|b| async move {
        let (_, bob, _) = world(&b).await;
        let s = start(&b, opts()).await;
        b.dir.set_status(bob, AccountStatus::Disabled).await.unwrap();
        let mut c = Client::secure_hello(s.addr, &s.cert_der).await;
        let a = c.cmd(&format!("AUTH PLAIN {}", plain("bob@example.net", "pw-bob-12345"))).await;
        let u = c.cmd(&format!("AUTH PLAIN {}", plain("nobody@example.net", "whatever-pw"))).await;
        let n = c.cmd(&format!("AUTH PLAIN {}", plain("not-an-address", "whatever-pw"))).await;
        for r in [&a, &u, &n] {
            assert!(r.starts_with("535 5.7.8"), "{r}");
        }
        assert_eq!(a, u, "a disabled account and an unknown one answer identically");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_user_cannot_send_as_someone_else() {
    for_each_backend(|b| async move {
        let (alice, bob, eve) = world(&b).await;
        let s = start(&b, opts()).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        // envelope sender: another account of the same tenant, another tenant, an unknown address, the null sender
        for from in ["bob@example.net", "eve@other.test", "nobody@example.net", "alice@elsewhere.example"] {
            let r = c.cmd(&format!("MAIL FROM:<{from}>")).await;
            assert!(r.starts_with("553 5.7.1"), "{from}: {r}");
        }
        assert!(c.cmd("MAIL FROM:<>").await.starts_with("553 5.7.1"));
        // an alias of the user is theirs
        let r = c.cmd("MAIL FROM:<ally@example.net>").await;
        assert!(r.starts_with("250"), "{r}");
        c.cmd("RSET").await;
        // header From: must also be the user
        for (hdr, want) in [
            ("From: bob@example.net", "550 5.7.1"),
            ("From: eve@other.test", "550 5.7.1"),
            ("From: alice@example.net, bob@example.net", "550 5.7.1"),
            ("From: alice@example.net\r\nFrom: bob@example.net", "550 5.7.1"),
            ("Subject: no from header", "550 5.7.1"),
        ] {
            let body = format!("{hdr}\r\n\r\nx");
            let r = c.mail("alice@example.net", &["bob@example.net"], body.as_bytes()).await;
            assert!(r.starts_with(want), "{hdr:?}: {r}");
        }
        assert!(b.inbox(bob, &s.blobs).await.is_empty() && b.inbox(alice, &s.blobs).await.is_empty());
        assert!(b.inbox(eve, &s.blobs).await.is_empty());
        // the user's own alias in the header is accepted
        let r = c.mail("alice@example.net", &["bob@example.net"], b"From: Ally <ally@example.net>\r\n\r\nx").await;
        assert!(r.starts_with("250"), "{r}");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn mail_for_a_remote_recipient_is_a_durable_job_before_the_250() {
    for_each_backend(|b| async move {
        let (_, bob, _) = world(&b).await;
        let s = start(&b, opts()).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        assert_eq!(b.count("jobs").await, 0);
        let r = c.mail("alice@example.net", &["far@away.example"], MSG.strip_suffix(b"\r\n").unwrap()).await;
        assert!(r.starts_with("250"), "{r}");
        // the reply has been read: the job already exists, exactly one
        assert_eq!(b.count("jobs").await, 1);
        let payload = b.fx.strings("SELECT CAST(payload AS TEXT) FROM jobs", &[]).await.remove(0);
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["envelope_from"], "alice@example.net");
        assert_eq!(v["recipients"], serde_json::json!(["far@away.example"]));
        assert_eq!(v["destination"], "away.example");
        assert_eq!(v["sender_domain"], "example.net");
        // the queued body is in the blob store and is the prepared message
        let hash = rusty_core::BlobHash::parse(v["blob"].as_str().unwrap()).unwrap();
        let body = s.blobs.get(&hash).await.unwrap();
        assert!(contains(&body, b"Subject: hi") && contains(&body, b"ESMTPSA"));
        assert_eq!(b.fx.strings("SELECT kind FROM jobs", &[]).await, ["outbound.send"]);
        // the job belongs to the SENDER's tenant
        assert_eq!(b.fx.scalar("SELECT count(*) FROM jobs j JOIN domains d ON d.id = j.domain_id WHERE d.name = 'example.net'", &[]).await, 1);

        // mixed: one local and two remote domains: local copy plus one job per destination domain
        let before = b.count("jobs").await;
        let r = c
            .mail("alice@example.net", &["bob@example.net", "a@one.example", "b@one.example", "c@two.example"], MSG.strip_suffix(b"\r\n").unwrap())
            .await;
        assert!(r.starts_with("250"), "{r}");
        assert_eq!(b.count("jobs").await, before + 2);
        assert_eq!(b.inbox(bob, &s.blobs).await.len(), 1);
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn implicit_tls_on_465_works_and_plaintext_is_dropped() {
    for_each_backend(|b| async move {
        let (_, bob, _) = world(&b).await;
        let mut o = opts();
        o.implicit_tls = true;
        let s = start(&b, o).await;
        // plaintext bytes on the TLS port: no greeting, the connection is dropped
        let mut p = Client::connect(s.addr).await;
        p.send(b"EHLO plain.example\r\n").await;
        assert!(p.closed_within(Duration::from_secs(15)).await, "plaintext on 465 must be dropped");
        // a TLS client sees the greeting, AUTH is offered at once, no STARTTLS
        let mut c = Client::connect_tls(s.addr, &s.cert_der).await;
        assert!(c.reply().await.starts_with("220 "));
        let ehlo = c.cmd("EHLO client.example.org").await;
        assert!(ehlo.contains("AUTH PLAIN LOGIN") && !ehlo.contains("STARTTLS"), "{ehlo}");
        assert!(c.cmd(&format!("AUTH PLAIN {}", plain("alice@example.net", ALICE_PW))).await.starts_with("235"));
        assert!(c.mail("alice@example.net", &["bob@example.net"], MSG.strip_suffix(b"\r\n").unwrap()).await.starts_with("250"));
        assert_eq!(b.inbox(bob, &s.blobs).await.len(), 1);
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_per_account_message_limit_gives_451() {
    for_each_backend(|b| async move {
        world(&b).await;
        let mut o = opts();
        o.per_hour = 2;
        let s = start(&b, o).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        for _ in 0..2 {
            let r = c.mail("alice@example.net", &["bob@example.net"], MSG.strip_suffix(b"
").unwrap()).await;
            assert!(r.starts_with("250"), "{r}");
        }
        let r = c.cmd("MAIL FROM:<alice@example.net>").await;
        assert!(r.starts_with("451 4.7.1"), "{r}");
        // another account is not limited by alice's use
        let mut c2 = Client::secure_hello(s.addr, &s.cert_der).await;
        assert!(c2.cmd(&format!("AUTH PLAIN {}", plain("bob@example.net", "pw-bob-12345"))).await.starts_with("235"));
        assert!(c2.cmd("MAIL FROM:<bob@example.net>").await.starts_with("250"));
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_recipient_cap_gives_452_across_local_and_remote() {
    for_each_backend(|b| async move {
        world(&b).await;
        let mut o = opts();
        o.max_recipients = 2;
        let s = start(&b, o).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        c.cmd("MAIL FROM:<alice@example.net>").await;
        assert!(c.cmd("RCPT TO:<bob@example.net>").await.starts_with("250"));
        assert!(c.cmd("RCPT TO:<far@away.example>").await.starts_with("250"));
        assert!(c.cmd("RCPT TO:<far2@away.example>").await.starts_with("452 4.5.3"));
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn an_unknown_local_user_is_refused_but_the_world_is_reachable() {
    for_each_backend(|b| async move {
        world(&b).await;
        let s = start(&b, opts()).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        c.cmd("MAIL FROM:<alice@example.net>").await;
        assert!(c.cmd("RCPT TO:<nobody@example.net>").await.starts_with("550 5.1.1"));
        assert!(c.cmd("RCPT TO:<anyone@gmail.example>").await.starts_with("250"));
        assert!(c.cmd("RCPT TO:<a b@gmail.example>").await.starts_with("501"));
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
async fn passwords_and_auth_blobs_never_reach_the_log() {
    for_each_backend(|b| async move {
        world(&b).await;
        let buf = Buf::default();
        let _guard = tracing::subscriber::set_default(rusty_svc::subscriber(LogLevel::Trace, LogFormat::Text, buf.clone()));
        let s = start(&b, opts()).await;
        let mut c = Client::secure_hello(s.addr, &s.cert_der).await;
        let wrong = plain("alice@example.net", "Wrong-Secret-9991");
        c.cmd(&format!("AUTH PLAIN {wrong}")).await;
        c.cmd("AUTH LOGIN").await;
        c.cmd(&b64(b"alice@example.net")).await;
        c.cmd(&b64(b"Login-Secret-7772")).await;
        let good = plain("alice@example.net", ALICE_PW);
        c.cmd(&format!("AUTH PLAIN {good}")).await;
        c.mail("alice@example.net", &["bob@example.net"], b"From: alice@example.net\r\n\r\nSECRET-BODY-5553").await;
        s.stop().await;
        let log = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        for token in ["Wrong-Secret-9991", "Login-Secret-7772", ALICE_PW, wrong.as_str(), good.as_str(), "SECRET-BODY-5553"] {
            assert!(!log.contains(token), "{token} reached the log:\n{log}");
        }
    })
    .await;
}

// ---------------------------------------------------------------- U5 review fixes (I2-I5, minors)

use rusty_smtp::outbound_queue::SubmitLimits;

const REMOTE: &[u8] = b"From: Alice <alice@example.net>\r\nTo: x@remote.test\r\nSubject: hi\r\n\r\nbody";

#[tokio::test]
async fn a_retried_submission_queues_the_remote_message_once() {
    for_each_backend(|b| async move {
        world(&b).await;
        let s = start(&b, opts()).await;
        // The same message twice (a client retrying after a 451): one job per destination, not two.
        for _ in 0..2 {
            let mut c = authed(s.addr, &s.cert_der).await;
            let r = c.mail("alice@example.net", &["x@remote.test", "y@other-remote.test"], REMOTE).await;
            assert!(r.starts_with("250"), "{r}");
        }
        assert_eq!(b.fx.scalar("SELECT count(*) FROM jobs WHERE kind = 'outbound.send'", &[]).await, 2, "{}", b.name);
        // A different message is a different submission.
        let mut c = authed(s.addr, &s.cert_der).await;
        let other = b"From: Alice <alice@example.net>\r\nTo: x@remote.test\r\nSubject: second\r\n\r\nbody";
        assert!(c.mail("alice@example.net", &["x@remote.test"], other).await.starts_with("250"));
        assert_eq!(b.fx.scalar("SELECT count(*) FROM jobs WHERE kind = 'outbound.send'", &[]).await, 3, "{}", b.name);
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_full_queue_or_budget_is_a_temporary_failure_and_one_user_cannot_starve_another() {
    for_each_backend(|b| async move {
        let (_alice, _bob, _) = world(&b).await;
        let dom = b.dir.find_domain("example.net").await.unwrap().unwrap().id;
        b.dir.create_account("carol", dom, Some("pw-carol-1234"), None).await.unwrap();
        let mut o = opts();
        o.submit_limits = Some(SubmitLimits { max_pending: 1, window_secs: 3600, max_jobs: 100, max_recipients: 100, max_bytes: 1 << 30 });
        let s = start(&b, o).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        assert!(c.mail("alice@example.net", &["x@remote.test"], REMOTE).await.starts_with("250"));
        let second = b"From: Alice <alice@example.net>\r\nTo: x@remote.test\r\nSubject: two\r\n\r\nbody";
        let r = c.mail("alice@example.net", &["x@remote.test"], second).await;
        assert!(r.starts_with("451"), "queue full is temporary, never a permanent 552: {r}");
        // Carol has her own share of the tenant's queue.
        let mut c2 = Client::secure_hello(s.addr, &s.cert_der).await;
        assert!(c2.cmd(&format!("AUTH PLAIN {}", plain("carol@example.net", "pw-carol-1234"))).await.starts_with("235"));
        let from_carol = b"From: carol@example.net\r\nTo: x@remote.test\r\nSubject: c\r\n\r\nbody";
        assert!(c2.mail("carol@example.net", &["x@remote.test"], from_carol).await.starts_with("250"));
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_submission_budget_counts_recipients_and_bytes_and_survives_a_restart() {
    for_each_backend(|b| async move {
        world(&b).await;
        let limits = SubmitLimits { max_pending: 100, window_secs: 3600, max_jobs: 100, max_recipients: 2, max_bytes: 1 << 30 };
        let mut o = opts();
        o.submit_limits = Some(limits);
        let s = start(&b, o).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        assert!(c.mail("alice@example.net", &["a@remote.test", "b@remote.test"], REMOTE).await.starts_with("250"));
        s.stop().await;
        // A new process (new in-memory state) still knows the budget: it lives in the queue's rows.
        let mut o = opts();
        o.submit_limits = Some(limits);
        let s = start(&b, o).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        let more = b"From: Alice <alice@example.net>\r\nTo: c@remote.test\r\nSubject: more\r\n\r\nbody";
        let r = c.mail("alice@example.net", &["c@remote.test"], more).await;
        assert!(r.starts_with("451"), "recipient budget used up: {r}");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn remote_recipients_are_refused_while_the_worker_is_disabled() {
    for_each_backend(|b| async move {
        world(&b).await;
        let mut o = opts();
        o.remote_enabled = false;
        let s = start(&b, o).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        assert!(c.cmd("MAIL FROM:<alice@example.net>").await.starts_with("250"));
        let r = c.cmd("RCPT TO:<x@remote.test>").await;
        assert!(r.starts_with("451 4.3.2"), "mail is never accepted into a queue nobody drains: {r}");
        // Local delivery is unaffected.
        assert!(c.cmd("RCPT TO:<bob@example.net>").await.starts_with("250"));
        assert_eq!(b.count("jobs").await, 0);
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_hosted_but_disabled_domain_is_not_queued_as_remote_and_lists_with_external_members_are_refused() {
    for_each_backend(|b| async move {
        let (_alice, _bob, _eve) = world(&b).await;
        let other = b.dir.find_domain("other.test").await.unwrap().unwrap().id;
        b.dir.set_domain_status(other, rusty_store::repo::DomainStatus::Disabled).await.unwrap();
        let s = start(&b, opts()).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        assert!(c.cmd("MAIL FROM:<alice@example.net>").await.starts_with("250"));
        let r = c.cmd("RCPT TO:<eve@other.test>").await;
        assert!(r.starts_with("550 5.1.2"), "a disabled hosted domain is not remote: {r}");
        // A list with an external member: refused, not silently reduced to its local members.
        let (alice, _) = (b.dir.find_account("alice@example.net").await.unwrap().unwrap().id, ());
        let dom = b.dir.find_domain("example.net").await.unwrap().unwrap().id;
        let list = b.dir.create_list("team", dom).await.unwrap();
        b.dir.add_list_member(list, Some(alice), None).await.unwrap();
        b.dir.add_list_member(list, None, Some("far@away.example")).await.unwrap();
        let r = c.cmd("RCPT TO:<team@example.net>").await;
        assert!(r.starts_with("550 5.7.2"), "{r}");
        assert_eq!(b.count("jobs").await, 0);
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_atomic_memory_limiter_does_not_overshoot_under_parallel_attempts() {
    use rusty_smtp::limits::{MemorySubmissionLimiter, SubmissionLimiter};
    use rusty_svc::SystemClock;
    let lim = Arc::new(MemorySubmissionLimiter::new(5, Arc::new(SystemClock::new())));
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let l = lim.clone();
        tasks.push(tokio::spawn(async move { l.try_record(AccountId(1)) }));
    }
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap() {
            ok += 1;
        }
    }
    assert_eq!(ok, 5, "exactly the budget passes, however many connections race");
}

#[tokio::test]
async fn a_retry_after_a_failed_queue_step_converges_without_duplicates() {
    for_each_backend(|b| async move {
        let (_alice, bob, _) = world(&b).await;
        let mut o = opts();
        o.submit_limits = Some(SubmitLimits { max_pending: 1, window_secs: 3600, max_jobs: 100, max_recipients: 100, max_bytes: 1 << 30 });
        let s = start(&b, o).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        assert!(c.mail("alice@example.net", &["x@remote.test"], REMOTE).await.starts_with("250")); // fills the queue
        let mixed = b"From: Alice <alice@example.net>\r\nTo: bob@example.net, y@remote.test\r\nSubject: mixed\r\n\r\nbody";
        let r = c.mail("alice@example.net", &["bob@example.net", "y@remote.test"], mixed).await;
        assert!(r.starts_with("451"), "the queue step failed: temporary for the client: {r}");
        s.stop().await;
        // The client retries the same message later, against a server whose queue has room.
        let s = start(&b, opts()).await;
        let mut c = authed(s.addr, &s.cert_der).await;
        let r = c.mail("alice@example.net", &["bob@example.net", "y@remote.test"], mixed).await;
        assert!(r.starts_with("250"), "{r}");
        let copies = b.inbox(bob, &s.blobs).await.iter().filter(|m| contains(m, b"Subject: mixed")).count();
        assert_eq!(copies, 1, "{}: the local copy was stored once across both attempts", b.name);
        assert_eq!(b.fx.scalar("SELECT count(*) FROM jobs WHERE kind = 'outbound.send'", &[]).await, 2, "{}", b.name);
        s.stop().await;
    })
    .await;
}
