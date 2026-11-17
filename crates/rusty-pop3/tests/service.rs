//! The POP3 listener over a real database: a message delivered to the Inbox is downloaded and deleted by POP3 and is then gone for every other protocol; a locked account, a wrong password and
//! the throttle are refused as in IMAP; a password is never taken in clear text and `STLS` makes it possible; the limits (line, snapshot, time) hold.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rusty_core::config::StartupConfig;
use rusty_core::{AccountId, Flags};
use rusty_directory::Directory;
use rusty_pop3::{Limits, Pop3Handler, Port};
use rusty_store::fixtures::pg_db as pg;
use rusty_store::{BlobStore, FsBlobStore, Mailbox, Store, StoreConfig};
use rusty_svc::{BoxedStream, Context, FileSecretSource, PeerInfo, SessionHandler, TlsProvider};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const HOST: &str = "mail.test";
const IO_TIMEOUT: Duration = Duration::from_secs(10);

struct World {
    seed: Directory,
    store: Store,
    blobs: Arc<dyn BlobStore>,
    tmp: tempfile::TempDir,
    _db: pg::PgTestDb,
}

async fn world() -> World {
    let url = pg::pg_admin_url().expect("a Postgres test server (RUSTY_TEST_PG_URL)");
    let db = pg::PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    let seed_store = Store::open(StoreConfig::postgres(db.seed_url())).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("blobs");
    FsBlobStore::init(&root).await.unwrap();
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::open(&root).await.unwrap());
    World { seed: Directory::with_repo(seed_store.repos().directory.clone()), store, blobs, tmp, _db: db }
}

impl World {
    async fn ctx(&self) -> Context {
        let mut cfg = StartupConfig::dev(self.tmp.path());
        cfg.hostname = Some(HOST.into());
        let tls = TlsProvider::from_config(&cfg.tls, HOST, Arc::new(FileSecretSource::new())).unwrap();
        Context::new(self.store.repos(), self.blobs.clone(), Arc::new(cfg), tls).await.unwrap()
    }

    async fn account(&self, addr: &str, password: &str) -> AccountId {
        let (local, domain) = addr.split_once('@').unwrap();
        let dom = match self.seed.find_domain(domain).await.unwrap() {
            Some(d) => d.id,
            None => self.seed.create_domain(domain).await.unwrap(),
        };
        let acc = self.seed.create_account(local, dom, None, None).await.unwrap();
        self.seed.set_password(acc, password).await.unwrap();
        acc
    }

    /// Put `raw` in the Inbox of `acc`; the message id and the uid.
    async fn deliver(&self, ctx: &Context, acc: AccountId, raw: &[u8]) -> (i64, u32) {
        let mb = ctx.hub.open_for(acc).await.unwrap();
        let inbox = inbox_of(&mb).await;
        let r = mb.append(inbox, Bytes::copy_from_slice(raw), Flags::default()).await.unwrap();
        (r.id.0, r.uid)
    }
}

async fn inbox_of(mb: &Mailbox) -> rusty_core::FolderId {
    mb.list_folders().await.unwrap().into_iter().find(|f| f.role == "inbox").unwrap().id
}

async fn inbox_uids(mb: &Mailbox) -> Vec<u32> {
    let inbox = inbox_of(mb).await;
    mb.list_folder(inbox, 0, 1000).await.unwrap().into_iter().map(|m| m.uid).collect()
}

trait Rw: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Rw for T {}

struct Conn {
    io: Box<dyn Rw>,
    buf: Vec<u8>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

fn peer(tls: bool) -> PeerInfo {
    PeerInfo { addr: SocketAddr::from(([192, 0, 2, 9], 40000)), tls }
}

impl Conn {
    fn open(handler: Arc<Pop3Handler>, tls: bool) -> Conn {
        let (client, server) = tokio::io::duplex(1 << 21);
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let task = tokio::spawn(async move {
            let stream: BoxedStream = Box::new(server);
            handler.handle(stream, peer(tls), token).await;
        });
        Conn { io: Box::new(client), buf: Vec::new(), shutdown, task }
    }

    async fn send(&mut self, s: &str) {
        tokio::time::timeout(IO_TIMEOUT, self.io.write_all(s.as_bytes())).await.expect("write timed out").expect("write");
        let _ = self.io.flush().await;
    }

    async fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 16 * 1024];
        match tokio::time::timeout(IO_TIMEOUT, self.io.read(&mut chunk)).await {
            Err(_) => panic!("timed out waiting for the server; so far {:?}", String::from_utf8_lossy(&self.buf)),
            Ok(Ok(0)) | Ok(Err(_)) => false,
            Ok(Ok(n)) => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
        }
    }

    /// One line without CRLF; `None` when the server closed the connection.
    async fn try_line(&mut self) -> Option<String> {
        loop {
            if let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=i).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Some(String::from_utf8_lossy(&line).into_owned());
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    async fn line(&mut self) -> String {
        self.try_line().await.expect("the connection was closed while a line was awaited")
    }

    /// Send `cmd` and read its reply: the status line, and for a multi-line reply (`multi`) every line up to the lone dot.
    async fn cmd(&mut self, cmd: &str, multi: bool) -> Vec<String> {
        self.send(&format!("{cmd}\r\n")).await;
        let first = self.line().await;
        let mut lines = vec![first.clone()];
        if multi && first.starts_with("+OK") {
            loop {
                let l = self.line().await;
                let end = l == ".";
                lines.push(l);
                if end {
                    break;
                }
            }
        }
        lines
    }

    async fn status(&mut self, cmd: &str) -> String {
        self.cmd(cmd, false).await.remove(0)
    }

    async fn login(&mut self, user: &str, pw: &str) -> String {
        let u = self.status(&format!("USER {user}")).await;
        assert!(u.starts_with("+OK"), "{u}");
        self.status(&format!("PASS {pw}")).await
    }

    async fn closed(&mut self) -> bool {
        loop {
            if self.try_line().await.is_none() {
                return true;
            }
        }
    }
}

fn handler(ctx: Context, port: Port) -> Arc<Pop3Handler> {
    Arc::new(Pop3Handler::new(ctx, port))
}

fn handler_with(ctx: Context, port: Port, limits: Limits) -> Arc<Pop3Handler> {
    Arc::new(Pop3Handler::new(ctx, port).with_limits(limits))
}

const M1: &[u8] = b"From: a@remote.test\r\nTo: alice@one.test\r\nSubject: one\r\n\r\nHello\r\n.leading dot\r\nlast line\r\n";
const M2: &[u8] = b"From: b@remote.test\r\nSubject: two\r\n\r\nSecond\r\n";
const M3: &[u8] = b"From: c@remote.test\r\nSubject: three\r\n\r\nThird\r\n";

#[tokio::test]
async fn a_message_is_listed_downloaded_and_deleted_and_is_then_gone_for_every_other_protocol() {
    let w = world().await;
    let ctx = w.ctx().await;
    let acc = w.account("alice@one.test", "correct horse").await;
    let (_, uid1) = w.deliver(&ctx, acc, M1).await;
    let (_, uid2) = w.deliver(&ctx, acc, M2).await;
    let (_, uid3) = w.deliver(&ctx, acc, M3).await;

    let mut c = Conn::open(handler(ctx.clone(), Port::Implicit), true);
    assert_eq!(c.line().await, "+OK rusty POP3 ready");
    let hello = c.login("alice@one.test", "correct horse").await;
    let total = M1.len() + M2.len() + M3.len();
    assert_eq!(hello, format!("+OK maildrop has 3 messages ({total} octets)"));
    assert_eq!(c.status("STAT").await, format!("+OK 3 {total}"));
    let list = c.cmd("LIST", true).await;
    assert_eq!(list[1..4], [format!("1 {}", M1.len()), format!("2 {}", M2.len()), format!("3 {}", M3.len())]);
    // The UIDL is the UIDVALIDITY of the Inbox and the UID: the same ones IMAP shows.
    let mb = ctx.hub.open_for(acc).await.unwrap();
    let inbox = mb.list_folders().await.unwrap().into_iter().find(|f| f.role == "inbox").unwrap();
    let uidl = c.cmd("UIDL", true).await;
    assert_eq!(uidl[1..4], [format!("1 {}.{uid1}", inbox.uid_validity), format!("2 {}.{uid2}", inbox.uid_validity), format!("3 {}.{uid3}", inbox.uid_validity)]);

    // The first message, as the client reads it: dot-stuffed on the wire, and the stuffing taken off it is the stored message.
    let r = c.cmd("RETR 1", true).await;
    assert_eq!(r[0], format!("+OK {} octets", M1.len()));
    let mut back = Vec::new();
    for l in &r[1..r.len() - 1] {
        back.extend_from_slice(l.strip_prefix('.').unwrap_or(l).as_bytes());
        back.extend_from_slice(b"\r\n");
    }
    assert_eq!(back, M1);
    let top = c.cmd("TOP 3 0", true).await;
    assert!(top.contains(&"Subject: three".to_owned()) && !top.contains(&"Third".to_owned()), "{top:?}");

    // Nothing leaves before QUIT.
    assert_eq!(c.status("DELE 1").await, "+OK message 1 deleted");
    assert_eq!(c.status("DELE 2").await, "+OK message 2 deleted");
    assert_eq!(inbox_uids(&mb).await, vec![uid1, uid2, uid3], "marked, not removed");
    assert_eq!(c.status("QUIT").await, "+OK bye");
    assert!(c.closed().await);
    assert_eq!(inbox_uids(&mb).await, vec![uid3], "QUIT removed exactly the two marked");

    // A new session sees what is left, and nothing else of the account or of the mailbox.
    let mut c = Conn::open(handler(ctx.clone(), Port::Implicit), true);
    c.line().await;
    assert_eq!(c.login("alice@one.test", "correct horse").await, format!("+OK maildrop has 1 messages ({} octets)", M3.len()));
}

#[tokio::test]
async fn a_dropped_connection_removes_nothing_and_a_message_marked_deleted_in_imap_is_not_offered() {
    let w = world().await;
    let ctx = w.ctx().await;
    let acc = w.account("alice@one.test", "pw-123456").await;
    let (id1, uid1) = w.deliver(&ctx, acc, M1).await;
    let (id2, _) = w.deliver(&ctx, acc, M2).await;
    let mb = ctx.hub.open_for(acc).await.unwrap();
    // IMAP marked the second message \Deleted and has not expunged it.
    mb.set_flags(rusty_core::MessageId(id2), Flags::DELETED).await.unwrap();

    let mut c = Conn::open(handler(ctx.clone(), Port::Implicit), true);
    c.line().await;
    assert_eq!(c.login("alice@one.test", "pw-123456").await, format!("+OK maildrop has 1 messages ({} octets)", M1.len()));
    assert_eq!(c.status("DELE 1").await, "+OK message 1 deleted");
    // The connection ends without QUIT.
    drop(c.io);
    tokio::time::timeout(Duration::from_secs(5), c.task).await.unwrap().unwrap();
    assert!(inbox_uids(&mb).await.contains(&uid1), "a dropped connection deletes nothing");
    let _ = id1;
}

#[tokio::test]
async fn two_sessions_of_one_account_may_run_at_once_and_a_message_another_session_removed_is_gone() {
    let w = world().await;
    let ctx = w.ctx().await;
    let acc = w.account("alice@one.test", "pw-123456").await;
    let (_, uid1) = w.deliver(&ctx, acc, M1).await;
    let (_, uid2) = w.deliver(&ctx, acc, M2).await;
    let mut a = Conn::open(handler(ctx.clone(), Port::Implicit), true);
    let mut b = Conn::open(handler(ctx.clone(), Port::Implicit), true);
    a.line().await;
    b.line().await;
    assert!(a.login("alice@one.test", "pw-123456").await.starts_with("+OK"));
    assert!(b.login("alice@one.test", "pw-123456").await.starts_with("+OK"), "no exclusive lock: IMAP reads the same mailbox at once");
    assert_eq!(a.status("DELE 1").await, "+OK message 1 deleted");
    assert_eq!(a.status("QUIT").await, "+OK bye");
    // `b` still has the snapshot with message 1: it has been removed, so RETR says so and nothing breaks.
    let r = b.status("RETR 1").await;
    assert!(r.starts_with("-ERR"), "{r}");
    assert_eq!(b.status("STAT").await, format!("+OK 1 {}", M2.len()));
    assert_eq!(b.status("DELE 2").await, "+OK message 2 deleted");
    assert_eq!(b.status("QUIT").await, "+OK bye");
    let mb = ctx.hub.open_for(acc).await.unwrap();
    assert!(inbox_uids(&mb).await.is_empty(), "{uid1} {uid2}");
}

#[tokio::test]
async fn a_wrong_password_a_locked_account_and_an_unknown_one_are_refused_alike_and_the_throttle_ends_a_guessing_client() {
    let w = world().await;
    let ctx = w.ctx().await;
    let acc = w.account("alice@one.test", "pw-123456").await;
    let mut c = Conn::open(handler(ctx.clone(), Port::Implicit), true);
    c.line().await;
    assert_eq!(c.login("alice@one.test", "wrong").await, "-ERR [AUTH] invalid credentials");
    assert_eq!(c.login("nobody@one.test", "wrong").await, "-ERR [AUTH] invalid credentials", "an unknown account answers as a wrong password does");
    // Locked: the right password is refused too.
    w.seed.set_status(acc, rusty_directory::AccountStatus::Locked).await.unwrap();
    assert_eq!(c.login("alice@one.test", "pw-123456").await, "-ERR [AUTH] invalid credentials", "a locked account cannot sign in");
    assert!(c.closed().await, "three bad passwords end the session");
    w.seed.set_status(acc, rusty_directory::AccountStatus::Active).await.unwrap();
    // A client that keeps guessing meets the shared throttle, with the code a client understands.
    let mut delayed = false;
    for _ in 0..40 {
        let mut c = Conn::open(handler(ctx.clone(), Port::Implicit), true);
        c.line().await;
        let answer = c.login("alice@one.test", "guess").await;
        if answer.starts_with("-ERR [LOGIN-DELAY]") {
            delayed = true;
            assert!(c.closed().await);
            break;
        }
    }
    assert!(delayed, "the throttle never answered");
}

#[tokio::test]
async fn a_password_is_refused_in_clear_text_and_stls_makes_it_possible_and_starts_the_session_over() {
    let w = world().await;
    let ctx = w.ctx().await;
    w.account("alice@one.test", "pw-123456").await;
    let mut c = Conn::open(handler(ctx.clone(), Port::Starttls), false);
    c.line().await;
    assert_eq!(c.status("USER alice@one.test").await, "-ERR [AUTH] TLS is required before a password");
    let caps = c.cmd("CAPA", true).await;
    assert!(caps.contains(&"STLS".to_owned()) && !caps.contains(&"USER".to_owned()), "{caps:?}");
    // A user name given before STLS is not carried across it.
    assert_eq!(c.status("STLS").await, "+OK begin TLS negotiation");
    tls_upgrade(&mut c).await;
    let caps = c.cmd("CAPA", true).await;
    assert!(caps.contains(&"USER".to_owned()) && caps.contains(&"SASL PLAIN".to_owned()) && !caps.contains(&"STLS".to_owned()), "{caps:?}");
    assert_eq!(c.status("PASS pw-123456").await, "-ERR USER first");
    assert!(c.login("alice@one.test", "pw-123456").await.starts_with("+OK maildrop has 0 messages"));
    assert_eq!(c.status("QUIT").await, "+OK bye");
}

#[tokio::test]
async fn data_sent_right_after_stls_is_an_injection_attempt_and_ends_the_connection() {
    let w = world().await;
    let ctx = w.ctx().await;
    let mut c = Conn::open(handler(ctx, Port::Starttls), false);
    c.line().await;
    c.send("STLS\r\nUSER alice@one.test\r\n").await;
    assert_eq!(c.line().await, "+OK begin TLS negotiation");
    assert_eq!(c.line().await, "-ERR unexpected data after STLS");
    assert!(c.closed().await);
}

#[tokio::test]
async fn the_limits_hold_a_line_too_long_the_snapshot_cap_and_the_time_to_sign_in_and_to_be_silent() {
    let w = world().await;
    let ctx = w.ctx().await;
    let acc = w.account("alice@one.test", "pw-123456").await;
    for raw in [M1, M2, M3] {
        w.deliver(&ctx, acc, raw).await;
    }
    // A line over the limit ends the connection, and a missing newline cannot grow the buffer for ever.
    let mut c = Conn::open(handler(ctx.clone(), Port::Implicit), true);
    c.line().await;
    c.send(&format!("USER {}\r\n", "a".repeat(600))).await;
    assert_eq!(c.line().await, "-ERR line too long");
    assert!(c.closed().await);
    let mut c = Conn::open(handler(ctx.clone(), Port::Implicit), true);
    c.line().await;
    c.send(&"x".repeat(2000)).await;
    assert_eq!(c.line().await, "-ERR line too long");
    // Only the oldest `max_messages` are offered.
    let limits = Limits { max_messages: 2, ..Limits::default() };
    let mut c = Conn::open(handler_with(ctx.clone(), Port::Implicit, limits), true);
    c.line().await;
    assert_eq!(c.login("alice@one.test", "pw-123456").await, format!("+OK maildrop has 2 messages ({} octets)", M1.len() + M2.len()));
    // A connection that never signs in is ended, and so is a signed-in one that says nothing.
    let limits = Limits { login_timeout: Duration::from_millis(250), ..Limits::default() };
    let mut c = Conn::open(handler_with(ctx.clone(), Port::Implicit, limits), true);
    c.line().await;
    assert_eq!(c.line().await, "-ERR [SYS/TEMP] autologout");
    assert!(c.closed().await);
    let limits = Limits { idle_timeout: Duration::from_millis(300), ..Limits::default() };
    let mut c = Conn::open(handler_with(ctx.clone(), Port::Implicit, limits), true);
    c.line().await;
    assert!(c.login("alice@one.test", "pw-123456").await.starts_with("+OK"));
    assert_eq!(c.line().await, "-ERR [SYS/TEMP] autologout");
    assert!(c.closed().await);
    // The service shutting down says so.
    let mut c = Conn::open(handler(ctx.clone(), Port::Implicit), true);
    c.line().await;
    c.shutdown.cancel();
    assert_eq!(c.line().await, "-ERR [SYS/TEMP] the server is shutting down");
    tokio::time::timeout(Duration::from_secs(5), &mut c.task).await.unwrap().unwrap();
}

#[tokio::test]
async fn commands_may_be_pipelined_and_a_large_message_round_trips() {
    let w = world().await;
    let ctx = w.ctx().await;
    let acc = w.account("alice@one.test", "pw-123456").await;
    let mut big = b"Subject: big\r\n\r\n".to_vec();
    for i in 0..40_000 {
        big.extend_from_slice(format!(".line {i} of a long message\r\n").as_bytes());
    }
    w.deliver(&ctx, acc, &big).await;
    let mut c = Conn::open(handler(ctx.clone(), Port::Implicit), true);
    c.line().await;
    c.send("USER alice@one.test\r\nPASS pw-123456\r\nSTAT\r\n").await;
    assert_eq!(c.line().await, "+OK");
    assert!(c.line().await.starts_with("+OK maildrop has 1 messages"));
    assert_eq!(c.line().await, format!("+OK 1 {}", big.len()));
    let r = c.cmd("RETR 1", true).await;
    assert_eq!(r[0], format!("+OK {} octets", big.len()));
    assert_eq!(r.len(), 1 + 2 + 40_000 + 1, "status, two header lines, the body and the lone dot");
    assert_eq!(r[3], "..line 0 of a long message", "every body line began with a dot, so every one was stuffed");
    assert_eq!(c.status("QUIT").await, "+OK bye");
}

/// Run the TLS client handshake over the connection's pipe (the server side upgrades itself).
async fn tls_upgrade(c: &mut Conn) {
    use rustls::pki_types::{CertificateDer, ServerName};
    #[derive(Debug)]
    struct NoVerify;
    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: rustls::pki_types::UnixTime) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
        }
    }
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let placeholder: Box<dyn Rw> = Box::new(tokio::io::duplex(1).0);
    let plain = std::mem::replace(&mut c.io, placeholder);
    let tls = connector.connect(ServerName::try_from(HOST.to_owned()).unwrap(), plain).await.expect("tls handshake");
    c.io = Box::new(tls);
}

#[tokio::test]
async fn a_retr_asks_the_shared_memory_budget_first_and_a_service_short_of_it_says_busy_and_keeps_the_message() {
    let w = world().await;
    let ctx = w.ctx().await;
    let acc = w.account("alice@one.test", "pw-123456").await;
    w.deliver(&ctx, acc, M1).await;
    // A budget of nothing: no message can be made ready to send.
    let none = Arc::new(Pop3Handler::new(ctx.clone(), Port::Implicit).with_budget(rusty_svc::MemoryBudget::new_mib(0)));
    let mut c = Conn::open(none, true);
    c.line().await;
    assert!(c.login("alice@one.test", "pw-123456").await.starts_with("+OK"));
    assert_eq!(c.status("RETR 1").await, "-ERR [SYS/TEMP] busy, try again in a moment");
    assert_eq!(c.status("STAT").await, format!("+OK 1 {}", M1.len()), "the message is still offered");
    // Room for it: it is sent, and the reservation is given back when the reply is made, so a second one fits as well.
    let some = Arc::new(Pop3Handler::new(ctx.clone(), Port::Implicit).with_budget(rusty_svc::MemoryBudget::new_mib(1)));
    let mut c = Conn::open(some, true);
    c.line().await;
    assert!(c.login("alice@one.test", "pw-123456").await.starts_with("+OK"));
    for _ in 0..3 {
        let r = c.cmd("RETR 1", true).await;
        assert!(r[0].starts_with("+OK"), "{r:?}");
    }
}
