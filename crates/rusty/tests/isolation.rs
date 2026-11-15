//! Acceptance of tenant isolation at the PROTOCOL level (usable-mailserver plan, U8 scenario 4): the real daemon (dev profile, Postgres), real SMTP and IMAP
//! over the wire. Accounts A1 and A2 live in `a.test`, B1 in `b.test`. What one tenant's session can reach is checked from the client's side, with guessed
//! names, ids and addresses; and the other mailboxes are read back afterwards to prove nothing moved.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use common::*;
use rusty::daemon::{start, Running};
use rusty_core::config::Profile;
use rusty_svc::ListenerKind;
use tokio_util::sync::CancellationToken;

type Tls = BufReader<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

#[derive(Debug)]
struct NoVerify;
impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(&self, _: &rustls::pki_types::CertificateDer<'_>, _: &[rustls::pki_types::CertificateDer<'_>], _: &rustls::pki_types::ServerName<'_>, _: &[u8], _: rustls::pki_types::UnixTime) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &rustls::pki_types::CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(&self, _: &[u8], _: &rustls::pki_types::CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

fn tls(addr: SocketAddr) -> Tls {
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    let conn = rustls::ClientConnection::new(Arc::new(cfg), "localhost".try_into().unwrap()).unwrap();
    let sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    BufReader::new(rustls::StreamOwned::new(conn, sock))
}

/// An IMAP session over TLS (port 993) that has logged in.
struct Imap {
    io: Tls,
    n: u32,
}

impl Imap {
    fn open(addr: SocketAddr) -> Imap {
        let mut io = tls(addr);
        let mut greeting = String::new();
        io.read_line(&mut greeting).unwrap();
        assert!(greeting.starts_with("* OK"), "{greeting}");
        Imap { io, n: 0 }
    }

    /// Send a command; return every line up to and including the tagged answer.
    fn cmd(&mut self, line: &str) -> Vec<String> {
        self.n += 1;
        let tag = format!("t{}", self.n);
        self.io.get_mut().write_all(format!("{tag} {line}\r\n").as_bytes()).unwrap();
        self.io.get_mut().flush().unwrap();
        let mut out = Vec::new();
        loop {
            let mut l = String::new();
            if self.io.read_line(&mut l).unwrap_or(0) == 0 {
                out.push("<closed>".into());
                return out;
            }
            let done = l.starts_with(&format!("{tag} "));
            out.push(l.trim_end().to_owned());
            if done {
                return out;
            }
        }
    }

    fn login(addr: SocketAddr, user: &str, pw: &str) -> Imap {
        let mut s = Imap::open(addr);
        let r = s.cmd(&format!("LOGIN \"{user}\" \"{pw}\""));
        assert!(r.last().unwrap().contains("OK"), "login {user}: {r:?}");
        s
    }

    fn ok(r: &[String]) -> bool {
        r.last().is_some_and(|l| l.split(' ').nth(1) == Some("OK"))
    }
}

fn exists_of(r: &[String]) -> Option<u32> {
    r.iter().find_map(|l| l.strip_prefix("* ")?.strip_suffix(" EXISTS")?.parse().ok())
}

/// Deliver one message on port 25 (plain text, no authentication: what a remote MTA does). Returns the answer to the end of DATA, or the refusal.
fn deliver(addr: SocketAddr, from: &str, rcpt: &str, subject: &str) -> String {
    let mut sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut io = BufReader::new(sock.try_clone().unwrap());
    let read = |io: &mut BufReader<TcpStream>| {
        loop {
            let mut l = String::new();
            io.read_line(&mut l).unwrap();
            if l.as_bytes().get(3) != Some(&b'-') {
                return l;
            }
        }
    };
    let say = |sock: &mut TcpStream, io: &mut BufReader<TcpStream>, line: &str| {
        sock.write_all(format!("{line}\r\n").as_bytes()).unwrap();
        read(io)
    };
    read(&mut io);
    say(&mut sock, &mut io, "EHLO remote.test");
    let r = say(&mut sock, &mut io, &format!("MAIL FROM:<{from}>"));
    if !r.starts_with("250") {
        return r;
    }
    let r = say(&mut sock, &mut io, &format!("RCPT TO:<{rcpt}>"));
    if !r.starts_with("250") {
        return r;
    }
    say(&mut sock, &mut io, "DATA");
    let body = format!("From: {from}\r\nTo: {rcpt}\r\nSubject: {subject}\r\nMessage-ID: <{subject}@remote.test>\r\nDate: Mon, 09 Nov 2026 10:00:00 +0000\r\n\r\nbody of {subject}\r\n.");
    let r = say(&mut sock, &mut io, &body);
    say(&mut sock, &mut io, "QUIT");
    r
}

/// A submission session over implicit TLS (465), authenticated; the answer to each command.
struct Submission {
    io: Tls,
}

impl Submission {
    fn open(addr: SocketAddr, user: &str, pw: &str) -> Submission {
        let mut s = Submission { io: tls(addr) };
        let g = s.reply();
        assert!(g.starts_with("220"), "{g}");
        assert!(s.cmd("EHLO t.example").starts_with("250"));
        let blob = base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{pw}"));
        let a = s.cmd(&format!("AUTH PLAIN {blob}"));
        assert!(a.starts_with("235"), "auth {user}: {a}");
        s
    }

    fn reply(&mut self) -> String {
        loop {
            let mut l = String::new();
            self.io.read_line(&mut l).unwrap();
            if l.as_bytes().get(3) != Some(&b'-') {
                return l;
            }
        }
    }

    fn cmd(&mut self, line: &str) -> String {
        self.io.get_mut().write_all(format!("{line}\r\n").as_bytes()).unwrap();
        self.io.get_mut().flush().unwrap();
        self.reply()
    }
}

struct World {
    run: Running,
    token: CancellationToken,
    _pg: PgDev,
    _tmp: tempfile::TempDir,
}

impl World {
    fn imaps(&self) -> SocketAddr {
        addr(&self.run, ListenerKind::Imaps993)
    }
    fn smtp(&self) -> SocketAddr {
        addr(&self.run, ListenerKind::Smtp25)
    }
    fn submissions(&self) -> SocketAddr {
        addr(&self.run, ListenerKind::Submissions465)
    }
}

fn addr(r: &Running, kind: ListenerKind) -> SocketAddr {
    r.addrs.iter().find(|(k, _)| *k == kind).map(|(_, a)| *a).expect("listener present")
}

const PW: &str = "pw-accept-1234";

/// Two tenants, three accounts, one message each delivered through the real port 25.
async fn world() -> World {
    let tmp = private_tmp();
    let pg = pg_dev().await;
    let mut cfg = pg.config(&tmp.path().join("data"));
    cfg.outbound.enabled = true;
    let store = pg.open_store().await;
    let dir = rusty_directory::Directory::with_repo(store.repos().directory.clone());
    let a = dir.create_domain("a.test").await.unwrap();
    let b = dir.create_domain("b.test").await.unwrap();
    for (local, dom) in [("alice", a), ("amy", a), ("bob", b)] {
        dir.create_account(local, dom, Some(PW), None).await.unwrap();
    }
    store.close().await.unwrap();
    let token = CancellationToken::new();
    let run = start(cfg, Profile::Dev, token.clone()).await.unwrap();
    World { run, token, _pg: pg, _tmp: tmp }
}

async fn stop(w: World) {
    w.token.cancel();
    w.run.wait().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_tenants_imap_session_reaches_only_its_own_mailbox() {
    let w = world().await;
    let (smtp, imaps) = (w.smtp(), w.imaps());
    tokio::task::spawn_blocking(move || {
        for (rcpt, n) in [("alice@a.test", 2), ("amy@a.test", 3), ("bob@b.test", 4)] {
            for i in 0..n {
                let r = deliver(smtp, "sender@remote.test", rcpt, &format!("{}-{i}", rcpt.split('@').next().unwrap()));
                assert!(r.starts_with("250"), "{rcpt}: {r}");
            }
        }
        // Each sees its own count.
        for (user, n) in [("alice@a.test", 2), ("amy@a.test", 3), ("bob@b.test", 4)] {
            let mut s = Imap::login(imaps, user, PW);
            assert_eq!(exists_of(&s.cmd("SELECT INBOX")), Some(n), "{user}");
        }
        // alice reaches for what is not hers.
        let mut alice = Imap::login(imaps, "alice@a.test", PW);
        assert_eq!(exists_of(&alice.cmd("SELECT INBOX")), Some(2));
        for guess in ["amy@a.test", "bob@b.test", "user/amy", "user/amy@a.test/INBOX", "#user/amy/INBOX", "~amy/INBOX", "../amy/INBOX", "..\\amy\\INBOX", "INBOX/../../amy", "other users/amy", "#shared/amy", "a.test/amy/INBOX", "/amy/INBOX", "amy/INBOX", "INBOX.amy", "Shared/amy"] {
            let r = alice.cmd(&format!("SELECT \"{guess}\""));
            assert!(!Imap::ok(&r) || exists_of(&r).is_none_or(|n| n == 0), "SELECT {guess:?} reached something: {r:?}");
            let r = alice.cmd(&format!("STATUS \"{guess}\" (MESSAGES)"));
            assert!(!r.iter().any(|l| l.contains("MESSAGES 3") || l.contains("MESSAGES 4")), "STATUS {guess:?} leaked a count: {r:?}");
            let r = alice.cmd(&format!("LIST \"\" \"{guess}\""));
            assert!(!r.iter().any(|l| l.starts_with("* LIST") && l.to_lowercase().contains("amy")), "LIST {guess:?}: {r:?}");
        }
        // The namespace and the folder list hold nothing of anyone else.
        let all = alice.cmd("LIST \"\" \"*\"");
        assert!(!all.iter().any(|l| l.contains("amy") || l.contains("bob")), "{all:?}");
        let ns = alice.cmd("NAMESPACE");
        assert!(!ns.iter().any(|l| l.contains("amy") || l.contains("bob")), "{ns:?}");
        // Message numbers and UIDs outside alice's mailbox are empty, never someone else's mail.
        assert_eq!(exists_of(&alice.cmd("SELECT INBOX")), Some(2));
        for set in ["3", "4", "5:9", "99999", "1000000:*"] {
            let r = alice.cmd(&format!("FETCH {set} (BODY[HEADER.FIELDS (SUBJECT)])"));
            assert!(!r.iter().any(|l| l.contains("amy-") || l.contains("bob-")), "FETCH {set}: {r:?}");
            let r = alice.cmd(&format!("UID FETCH {set} (BODY[HEADER.FIELDS (SUBJECT)])"));
            assert!(!r.iter().any(|l| l.contains("amy-") || l.contains("bob-")), "UID FETCH {set}: {r:?}");
        }
        let r = alice.cmd("UID FETCH 1:* (BODY[HEADER.FIELDS (SUBJECT)])");
        assert!(r.iter().filter(|l| l.contains("alice-")).count() == 2 && !r.iter().any(|l| l.contains("amy-") || l.contains("bob-")), "{r:?}");
        // A search cannot see across either.
        let r = alice.cmd("SEARCH SUBJECT \"amy-\"");
        assert!(r.iter().any(|l| l == "* SEARCH"), "no hit for another mailbox's subject: {r:?}");
        let r = alice.cmd("SEARCH SUBJECT \"bob-\"");
        assert!(r.iter().any(|l| l == "* SEARCH"), "{r:?}");
        // COPY, APPEND and CREATE name only alice's own folders: a path that reaches for another tenant creates nothing there.
        for target in ["amy/INBOX", "../amy/INBOX", "bob@b.test"] {
            let _ = alice.cmd(&format!("COPY 1 \"{target}\""));
            let _ = alice.cmd(&format!("CREATE \"{target}\""));
        }
        // Nothing moved: every mailbox holds exactly what was delivered.
        for (user, n) in [("alice@a.test", 2), ("amy@a.test", 3), ("bob@b.test", 4)] {
            let mut s = Imap::login(imaps, user, PW);
            let r = s.cmd("SELECT INBOX");
            assert_eq!(exists_of(&r), Some(n), "{user} after alice's attempts");
            let all = s.cmd("LIST \"\" \"*\"");
            assert!(!all.iter().any(|l| l.to_lowercase().contains("alice")), "{user} gained a folder of alice: {all:?}");
        }
    })
    .await
    .unwrap();
    stop(w).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authenticated_sender_cannot_send_as_another_user_or_tenant() {
    let w = world().await;
    let addr = w.submissions();
    tokio::task::spawn_blocking(move || {
        let mut s = Submission::open(addr, "alice@a.test", PW);
        for from in ["bob@b.test", "amy@a.test", "postmaster@b.test", "root@a.test", "alice@b.test", "alice@a.test.evil.test", "<alice@a.test>@b.test"] {
            let r = s.cmd(&format!("MAIL FROM:<{from}>"));
            assert!(r.starts_with('5'), "alice must not send as {from}: {r}");
            let _ = s.cmd("RSET");
        }
        // Her own address works, in any case.
        assert!(s.cmd("MAIL FROM:<alice@a.test>").starts_with("250"));
        assert!(s.cmd("RSET").starts_with("250"));
        assert!(s.cmd("MAIL FROM:<ALICE@A.TEST>").starts_with("250"));
        assert!(s.cmd("RSET").starts_with("250"));
        // The header From counts as well: her envelope address does not license someone else's name in the message.
        for forged in ["bob@b.test", "amy@a.test", "Bob <bob@b.test>", "alice@a.test, bob@b.test"] {
            assert!(s.cmd("MAIL FROM:<alice@a.test>").starts_with("250"));
            assert!(s.cmd("RCPT TO:<amy@a.test>").starts_with("250"));
            assert!(s.cmd("DATA").starts_with("354"));
            let body = format!("From: {forged}\r\nTo: amy@a.test\r\nSubject: forged\r\nMessage-ID: <f@a.test>\r\nDate: Mon, 09 Nov 2026 10:00:00 +0000\r\n\r\nx\r\n.");
            let r = s.cmd(&body);
            assert!(r.starts_with('5'), "a message from alice with the header From {forged:?} was accepted: {r}");
            let _ = s.cmd("RSET");
        }
        // And her own name goes through to a local recipient.
        assert!(s.cmd("MAIL FROM:<alice@a.test>").starts_with("250"));
        assert!(s.cmd("RCPT TO:<amy@a.test>").starts_with("250"));
        assert!(s.cmd("DATA").starts_with("354"));
        let ok = "From: Alice <alice@a.test>\r\nTo: amy@a.test\r\nSubject: fine\r\nMessage-ID: <g@a.test>\r\nDate: Mon, 09 Nov 2026 10:00:00 +0000\r\n\r\nx\r\n.";
        let r = s.cmd(ok);
        assert!(r.starts_with("250"), "her own name was refused: {r}");
    })
    .await
    .unwrap();
    stop(w).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_open_port_relays_nothing_and_accepts_no_mail_for_a_disabled_account() {
    let w = world().await;
    let smtp = w.smtp();
    let pg_store = w._pg.open_store().await;
    let dir = rusty_directory::Directory::with_repo(pg_store.repos().directory.clone());
    let amy = dir.find_account("amy@a.test").await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || {
        // Unauthenticated, to a name that is not ours: no relay, whatever the claimed sender.
        for (from, rcpt) in [("alice@a.test", "victim@elsewhere.test"), ("sender@remote.test", "victim@elsewhere.test"), ("bob@b.test", "victim@remote.test")] {
            let r = deliver(smtp, from, rcpt, "relay-probe");
            assert!(r.starts_with('5') || r.starts_with('4'), "{from} -> {rcpt} must not be accepted: {r}");
        }
        // A local recipient is accepted; an unknown one in a local domain is refused.
        assert!(deliver(smtp, "sender@remote.test", "amy@a.test", "ok-1").starts_with("250"));
        assert!(deliver(smtp, "sender@remote.test", "nobody@a.test", "no-user").starts_with('5'));
    })
    .await
    .unwrap();
    dir.set_status(amy.id, rusty_directory::AccountStatus::Disabled).await.unwrap();
    pg_store.close().await.unwrap();
    let smtp = w.smtp();
    tokio::task::spawn_blocking(move || {
        let r = deliver(smtp, "sender@remote.test", "amy@a.test", "to-disabled");
        assert!(r.starts_with('5') || r.starts_with('4'), "a disabled account accepts no mail: {r}");
    })
    .await
    .unwrap();
    stop(w).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_password_never_opens_a_session_and_repeated_guessing_is_throttled() {
    let w = world().await;
    let imaps = w.imaps();
    tokio::task::spawn_blocking(move || {
        let mut refused = 0;
        let mut throttled = false;
        for i in 0..40 {
            let mut s = Imap::open(imaps);
            let r = s.cmd(&format!("LOGIN \"alice@a.test\" \"guess-{i}\""));
            let last = r.last().cloned().unwrap_or_default();
            assert!(!Imap::ok(&r), "a wrong password logged in: {r:?}");
            refused += 1;
            if last.contains("THROTTLED") || last.contains("TOOMANY") || last.contains("LIMIT") || r.iter().any(|l| l == "<closed>") || last.to_lowercase().contains("too many") {
                throttled = true;
                break;
            }
        }
        assert!(throttled, "{refused} wrong passwords in a row were never throttled");
        // While throttled, even the right password is not accepted from this address.
        let mut s = Imap::open(imaps);
        let r = s.cmd(&format!("LOGIN \"alice@a.test\" \"{PW}\""));
        assert!(!Imap::ok(&r), "the right password got through while throttled: {r:?}");
    })
    .await
    .unwrap();
    stop(w).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_commands_and_messages_are_refused_without_hurting_the_service() {
    let w = world().await;
    let (imaps, smtp) = (w.imaps(), w.smtp());
    tokio::task::spawn_blocking(move || {
        // An IMAP line of 2 MiB with no end: the session is closed or refused, and the service keeps answering others.
        let mut s = Imap::open(imaps);
        let junk = vec![b'A'; 2 * 1024 * 1024];
        let _ = s.io.get_mut().write_all(b"x1 LOGIN ");
        let _ = s.io.get_mut().write_all(&junk);
        let _ = s.io.get_mut().write_all(b"\r\n");
        let _ = s.io.get_mut().flush();
        let mut rest = Vec::new();
        let _ = s.io.read_to_end(&mut rest);
        assert!(!String::from_utf8_lossy(&rest).contains("x1 OK"));
        // A 100 KiB SMTP command line is refused.
        let mut sock = TcpStream::connect(smtp).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut io = BufReader::new(sock.try_clone().unwrap());
        let mut l = String::new();
        io.read_line(&mut l).unwrap();
        let _ = sock.write_all(format!("MAIL FROM:<{}@remote.test>\r\n", "a".repeat(100_000)).as_bytes());
        let mut l = String::new();
        let _ = io.read_line(&mut l);
        assert!(l.starts_with('5') || l.starts_with('4') || l.is_empty(), "{l}");
        // A message over the size limit is refused at DATA time and nothing is stored.
        let mut sock = TcpStream::connect(smtp).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut io = BufReader::new(sock.try_clone().unwrap());
        let mut l = String::new();
        io.read_line(&mut l).unwrap();
        for line in ["EHLO remote.test", "MAIL FROM:<sender@remote.test>", "RCPT TO:<alice@a.test>", "DATA"] {
            sock.write_all(format!("{line}\r\n").as_bytes()).unwrap();
            loop {
                let mut r = String::new();
                io.read_line(&mut r).unwrap();
                if r.as_bytes().get(3) != Some(&b'-') {
                    break;
                }
            }
        }
        let line = format!("{}\r\n", "x".repeat(990));
        let mut refused = false;
        for _ in 0..(12 * 1024) {
            if sock.write_all(line.as_bytes()).is_err() {
                refused = true;
                break;
            }
        }
        let _ = sock.write_all(b".\r\n");
        let mut r = String::new();
        let _ = io.read_line(&mut r);
        assert!(refused || r.starts_with('5') || r.starts_with('4'), "a 12 MiB message was accepted: {r}");
        // The service still works.
        assert!(deliver(smtp, "sender@remote.test", "alice@a.test", "after-abuse").starts_with("250"));
        let mut a = Imap::login(imaps, "alice@a.test", PW);
        assert_eq!(exists_of(&a.cmd("SELECT INBOX")), Some(1), "only the small message was stored");
    })
    .await
    .unwrap();
    stop(w).await;
}
