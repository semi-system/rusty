//! The ManageSieve listener over a real database: scripts are put, checked, listed, read, activated, renamed and removed by a client that signed in with `AUTHENTICATE PLAIN` inside TLS (own or app
//! password); the parser refuses what it must; a password is never taken in clear text; the limits (command, script, time, failures) hold; the scripts are those the webmail and the delivery see.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::prelude::{Engine, BASE64_STANDARD};
use rusty_core::config::StartupConfig;
use rusty_core::AccountId;
use rusty_directory::Directory;
use rusty_managesieve::{Limits, ManageSieveHandler};
use rusty_store::fixtures::pg_db as pg;
use rusty_store::{BlobStore, FsBlobStore, Store, StoreConfig};
use rusty_svc::{BoxedStream, Context, FileSecretSource, PeerInfo, SessionHandler, TlsProvider};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const HOST: &str = "mail.test";
const IO_TIMEOUT: Duration = Duration::from_secs(10);

struct World {
    seed: Directory,
    seed_store: Store,
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
    World { seed: Directory::with_repo(seed_store.repos().directory.clone()), seed_store, store, blobs, tmp, _db: db }
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
}

trait Rw: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Rw for T {}

struct Conn {
    io: Box<dyn Rw>,
    buf: Vec<u8>,
    shutdown: CancellationToken,
}

fn peer(tls: bool) -> PeerInfo {
    PeerInfo { addr: SocketAddr::from(([192, 0, 2, 9], 40000)), tls }
}

impl Conn {
    fn open(handler: Arc<ManageSieveHandler>, tls: bool) -> Conn {
        let (client, server) = tokio::io::duplex(1 << 21);
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        tokio::spawn(async move {
            let stream: BoxedStream = Box::new(server);
            handler.handle(stream, peer(tls), token).await;
        });
        Conn { io: Box::new(client), buf: Vec::new(), shutdown }
    }

    async fn send(&mut self, s: &str) {
        tokio::time::timeout(IO_TIMEOUT, self.io.write_all(s.as_bytes())).await.expect("write timed out").expect("write");
        let _ = self.io.flush().await;
    }

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
            let mut chunk = [0u8; 16 * 1024];
            match tokio::time::timeout(IO_TIMEOUT, self.io.read(&mut chunk)).await {
                Err(_) => panic!("timed out; so far {:?}", String::from_utf8_lossy(&self.buf)),
                Ok(Ok(0)) | Ok(Err(_)) => return None,
                Ok(Ok(n)) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    async fn line(&mut self) -> String {
        self.try_line().await.expect("the connection was closed while a line was awaited")
    }

    /// Lines up to and including the status line (`OK`, `NO` or `BYE`).
    async fn reply(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            let l = self.line().await;
            let last = l.starts_with("OK") || l.starts_with("NO") || l.starts_with("BYE");
            lines.push(l);
            if last {
                return lines;
            }
        }
    }

    async fn cmd(&mut self, c: &str) -> Vec<String> {
        self.send(&format!("{c}\r\n")).await;
        self.reply().await
    }

    async fn status(&mut self, c: &str) -> String {
        self.cmd(c).await.pop().unwrap()
    }

    async fn closed(&mut self) -> bool {
        while self.try_line().await.is_some() {}
        true
    }
}

fn plain(user: &str, pw: &str) -> String {
    BASE64_STANDARD.encode(format!("\0{user}\0{pw}"))
}

async fn signed_in(handler: &Arc<ManageSieveHandler>, user: &str, pw: &str) -> Conn {
    let mut c = Conn::open(handler.clone(), true);
    c.reply().await;
    let r = c.status(&format!("AUTHENTICATE \"PLAIN\" \"{}\"", plain(user, pw))).await;
    assert!(r.starts_with("OK"), "{r}");
    c
}

fn handler(ctx: Context) -> Arc<ManageSieveHandler> {
    Arc::new(ManageSieveHandler::new(ctx))
}

fn put(name: &str, script: &str) -> String {
    format!("PUTSCRIPT \"{name}\" {{{}+}}\r\n{script}", script.len())
}

const GOOD: &str = "require \"fileinto\";\nif header :contains \"list-id\" \"lists.example.org\" { fileinto \"Lists\"; }\n";

#[tokio::test]
async fn the_greeting_offers_starttls_and_no_password_before_it_and_a_password_is_refused_in_clear_text() {
    let w = world().await;
    let ctx = w.ctx().await;
    w.account("alice@one.test", "pw-123456").await;
    let mut c = Conn::open(handler(ctx), false);
    let greeting = c.reply().await;
    assert!(greeting.contains(&"\"IMPLEMENTATION\" \"Rusty ManageSieve\"".to_owned()), "{greeting:?}");
    assert!(greeting.contains(&"\"STARTTLS\"".to_owned()) && greeting.contains(&"\"SASL\" \"\"".to_owned()), "{greeting:?}");
    assert!(greeting.iter().any(|l| l.starts_with("\"SIEVE\" \"") && l.contains("fileinto")), "{greeting:?}");
    let r = c.status(&format!("AUTHENTICATE \"PLAIN\" \"{}\"", plain("alice@one.test", "pw-123456"))).await;
    assert!(r.starts_with("NO (ENCRYPT-NEEDED)"), "{r}");
    assert_eq!(c.status("LISTSCRIPTS").await, "NO \"authenticate first\"");
    assert_eq!(c.status("NOOP").await, "OK \"Done\"");
    assert_eq!(c.status("NOOP \"t1\"").await, "OK (TAG \"t1\") \"Done\"");
    assert_eq!(c.status("LOGOUT").await, "OK \"Logout completed\"");
    assert!(c.closed().await);
}

#[tokio::test]
async fn starttls_starts_the_session_over_and_then_a_password_is_taken() {
    let w = world().await;
    let ctx = w.ctx().await;
    w.account("alice@one.test", "pw-123456").await;
    let mut c = Conn::open(handler(ctx), false);
    c.reply().await;
    assert_eq!(c.status("STARTTLS").await, "OK \"Begin TLS negotiation now\"");
    tls_upgrade(&mut c).await;
    let caps = c.reply().await;
    assert!(caps.contains(&"\"SASL\" \"PLAIN\"".to_owned()) && !caps.contains(&"\"STARTTLS\"".to_owned()), "{caps:?}");
    // The response may come after an empty challenge, as one string.
    c.send("AUTHENTICATE \"PLAIN\"\r\n").await;
    assert_eq!(c.line().await, "\"\"");
    c.send(&format!("\"{}\"\r\n", plain("alice@one.test", "pw-123456"))).await;
    assert_eq!(c.reply().await.pop().unwrap(), "OK \"Logged in\"");
    assert_eq!(c.status("AUTHENTICATE \"PLAIN\"").await, "NO \"already authenticated\"");
    assert_eq!(c.status("STARTTLS").await, "NO \"TLS is already on\"");
}

#[tokio::test]
async fn data_sent_right_after_starttls_is_an_injection_attempt_and_ends_the_connection() {
    let w = world().await;
    let ctx = w.ctx().await;
    let mut c = Conn::open(handler(ctx), false);
    c.reply().await;
    c.send("STARTTLS\r\nLISTSCRIPTS\r\n").await;
    assert_eq!(c.line().await, "OK \"Begin TLS negotiation now\"");
    assert!(c.closed().await);
}

#[tokio::test]
async fn scripts_are_checked_by_the_parser_kept_listed_read_activated_renamed_and_removed() {
    let w = world().await;
    let ctx = w.ctx().await;
    let acc = w.account("alice@one.test", "pw-123456").await;
    let h = handler(ctx.clone());
    let mut c = signed_in(&h, "alice@one.test", "pw-123456").await;
    // A script the parser refuses is not kept, and the answer says where.
    let bad = c.status(&put("main", "require \"fileinto\";\nif header :contains \"subject\" \"x\" {\n")).await;
    assert!(bad.starts_with("NO \"line "), "{bad}");
    assert_eq!(c.cmd("LISTSCRIPTS").await, ["OK \"Listscripts completed\""]);
    assert!(c.status("CHECKSCRIPT {4+}\r\nkeep").await.starts_with("NO"), "no semicolon");
    assert_eq!(c.status("CHECKSCRIPT {5+}\r\nkeep;").await, "OK \"Script is valid\"");
    // A good one is kept and read back as it was put; putting again replaces it.
    assert_eq!(c.status(&put("main", GOOD)).await, "OK \"Putscript completed\"");
    assert_eq!(c.status(&put("main", &format!("{GOOD}stop;\n"))).await, "OK \"Putscript completed\"");
    let got = c.cmd("GETSCRIPT \"main\"").await;
    assert_eq!(got[0], format!("{{{}}}", GOOD.len() + 6));
    assert_eq!(got.last().unwrap(), "OK \"Getscript completed\"");
    assert_eq!(c.status("GETSCRIPT \"nothing\"").await, "NO (NONEXISTENT) \"there is no such script\"");
    // The delivery sees what the client put once it is active; one runs at a time; an active one is not removed.
    assert_eq!(c.status("SETACTIVE \"main\"").await, "OK \"Setactive completed\"");
    assert!(ctx.sieve.active_body(acc).await.unwrap().unwrap().ends_with("stop;\n"));
    assert_eq!(c.status(&put("second", "keep;")).await, "OK \"Putscript completed\"");
    assert_eq!(c.status("SETACTIVE \"second\"").await, "OK \"Setactive completed\"");
    assert_eq!(c.cmd("LISTSCRIPTS").await, ["\"main\"", "\"second\" ACTIVE", "OK \"Listscripts completed\""]);
    assert_eq!(c.status("DELETESCRIPT \"second\"").await, "NO (ACTIVE) \"the script is active\"");
    assert_eq!(c.status("RENAMESCRIPT \"second\" \"main\"").await, "NO (ALREADYEXISTS) \"a script has that name\"");
    assert_eq!(c.status("RENAMESCRIPT \"second\" \"third\"").await, "OK \"Renamescript completed\"");
    assert_eq!(c.cmd("LISTSCRIPTS").await, ["\"main\"", "\"third\" ACTIVE", "OK \"Listscripts completed\""], "the flag moved with the name");
    assert_eq!(c.status("SETACTIVE \"\"").await, "OK \"Setactive completed\"");
    assert_eq!(ctx.sieve.active_body(acc).await.unwrap(), None);
    assert_eq!(c.status("DELETESCRIPT \"third\"").await, "OK \"Deletescript completed\"");
    assert_eq!(c.status("DELETESCRIPT \"third\"").await, "NO (NONEXISTENT) \"there is no such script\"");
    assert_eq!(c.status("SETACTIVE \"third\"").await, "NO (NONEXISTENT) \"there is no such script\"");
    // Size and number are bounded; HAVESPACE says so before the upload.
    assert_eq!(c.status("HAVESPACE \"x\" 70000").await, "NO (QUOTA/MAXSIZE) \"the script would be too large\"");
    assert_eq!(c.status("HAVESPACE \"x\" 100").await, "OK \"There is room\"");
    for i in 0..9 {
        assert_eq!(c.status(&put(&format!("s{i}"), "keep;")).await, "OK \"Putscript completed\"");
    }
    assert_eq!(c.status(&put("one-too-many", "keep;")).await, "NO (QUOTA/MAXSCRIPTS) \"too many scripts\"");
    assert_eq!(c.status("HAVESPACE \"one-too-many\" 10").await, "NO (QUOTA/MAXSCRIPTS) \"too many scripts\"");
    assert_eq!(c.status("HAVESPACE \"s1\" 10").await, "OK \"There is room\"", "a name that exists is replaced, not added");
    // The names the webmail would refuse are refused here too; a script that is not text is.
    assert!(c.status(&put("a\u{1}b", "keep;")).await.starts_with("NO"));
    assert!(c.status("PUTSCRIPT \"\" {5+}\r\nkeep;").await.starts_with("NO"));
}

#[tokio::test]
async fn a_script_put_here_is_the_one_the_webmail_lists_and_another_account_sees_none_of_it() {
    let w = world().await;
    let ctx = w.ctx().await;
    let alice = w.account("alice@one.test", "pw-123456").await;
    let bob = w.account("bob@one.test", "other-pw-1234").await;
    let h = handler(ctx.clone());
    let mut c = signed_in(&h, "alice@one.test", "pw-123456").await;
    assert_eq!(c.status(&put("mine", "keep;")).await, "OK \"Putscript completed\"");
    assert_eq!(ctx.sieve.list(alice).await.unwrap().len(), 1);
    assert!(ctx.sieve.list(bob).await.unwrap().is_empty());
    let mut b = signed_in(&h, "bob@one.test", "other-pw-1234").await;
    assert_eq!(b.cmd("LISTSCRIPTS").await, ["OK \"Listscripts completed\""]);
    assert_eq!(b.status("GETSCRIPT \"mine\"").await, "NO (NONEXISTENT) \"there is no such script\"");
    assert_eq!(b.status("DELETESCRIPT \"mine\"").await, "NO (NONEXISTENT) \"there is no such script\"");
}

#[tokio::test]
async fn an_app_password_signs_in_and_a_wrong_one_does_not_and_the_connection_is_closed_after_five_failures() {
    let w = world().await;
    let ctx = w.ctx().await;
    let acc = w.account("alice@one.test", "pw-123456").await;
    let secret = "abcd-efgh-ijkl-mnop";
    let hash = rusty_directory::hash_app_password(secret).await.unwrap();
    w.seed_store.repos().app_passwords.create(acc, "thunderbird", &hash).await.unwrap();
    let h = handler(ctx);
    let mut c = signed_in(&h, "alice@one.test", secret).await;
    assert_eq!(c.cmd("LISTSCRIPTS").await, ["OK \"Listscripts completed\""]);
    let mut c = Conn::open(h.clone(), true);
    c.reply().await;
    for _ in 0..2 {
        let r = c.status(&format!("AUTHENTICATE \"PLAIN\" \"{}\"", plain("alice@one.test", "wrong-pw-1"))).await;
        assert_eq!(r, "NO \"the name or the password is not right\"");
    }
    // Malformed responses are refused as such and cost nothing.
    assert_eq!(c.status("AUTHENTICATE \"PLAIN\" \"!!!\"").await, "NO \"the response is not valid\"");
    assert_eq!(c.status(&format!("AUTHENTICATE \"PLAIN\" \"{}\"", BASE64_STANDARD.encode("other\0alice@one.test\0pw-123456"))).await, "NO \"acting as another user is not offered\"");
    assert_eq!(c.status("AUTHENTICATE \"CRAM-MD5\"").await, "NO \"only PLAIN is offered\"");
    // After a few failures the throttle answers, or the connection is closed: either way it ends.
    let mut ended = false;
    for _ in 0..8 {
        c.send(&format!("AUTHENTICATE \"PLAIN\" \"{}\"\r\n", plain("alice@one.test", "wrong-pw-1"))).await;
        let r = c.reply().await;
        if r.last().unwrap().starts_with("BYE") {
            ended = true;
            break;
        }
    }
    assert!(ended && c.closed().await, "the connection was never closed");
}

#[tokio::test]
async fn a_command_that_cannot_be_read_is_answered_and_closes_the_connection_and_a_big_script_is_refused_by_its_size() {
    let w = world().await;
    let ctx = w.ctx().await;
    w.account("alice@one.test", "pw-123456").await;
    let h = handler(ctx);
    let mut c = signed_in(&h, "alice@one.test", "pw-123456").await;
    assert_eq!(c.status("FROBNICATE").await, "NO \"unknown command\"");
    assert_eq!(c.status("GETSCRIPT").await, "NO \"wrong arguments\"");
    // A literal over the limit is refused before its bytes are read.
    c.send("PUTSCRIPT \"big\" {70000+}\r\n").await;
    let r = c.reply().await;
    assert!(r[0].starts_with("NO (QUOTA/MAXSIZE)"), "{r:?}");
    assert!(c.closed().await);
    let mut c = signed_in(&h, "alice@one.test", "pw-123456").await;
    c.send("PUTSCRIPT \"x\" {5}\r\nkeep;\r\n").await;
    let r = c.reply().await;
    assert!(r[0].starts_with("NO"), "{r:?}");
    assert!(c.line().await.starts_with("BYE"));
    assert!(c.closed().await);
    let mut c = signed_in(&h, "alice@one.test", "pw-123456").await;
    c.send(&format!("{}\r\n", "A".repeat(5000))).await;
    assert!(c.reply().await[0].starts_with("NO"));
    assert!(c.closed().await);
}

#[tokio::test]
async fn a_connection_that_does_not_sign_in_in_time_is_closed_and_shutdown_says_bye() {
    let w = world().await;
    let ctx = w.ctx().await;
    let quick = Arc::new(ManageSieveHandler::new(ctx.clone()).with_limits(Limits { login_timeout: Duration::from_millis(300), idle_timeout: Duration::from_secs(60) }));
    let mut c = Conn::open(quick, true);
    c.reply().await;
    assert_eq!(c.line().await, "BYE \"autologout\"");
    assert!(c.closed().await);
    let mut c = Conn::open(handler(ctx), true);
    c.reply().await;
    c.shutdown.cancel();
    assert_eq!(c.line().await, "BYE (TRYLATER) \"the server is shutting down\"");
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
