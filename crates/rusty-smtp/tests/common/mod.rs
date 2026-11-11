//! Harness of the rusty-smtp driver tests: a real store on every backend, a real listener on a
//! loopback port, a line-oriented SMTP client, and helpers to read back what was stored.
#![allow(dead_code)]

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rusty_core::config::StartupConfig;
use rusty_core::{AccountId, FolderId};
use rusty_directory::Directory;
use rusty_smtp::inbound::{InboundHandler, InboundOptions, SubmissionDeps};
use rusty_smtp::limits::MemorySubmissionLimiter;
use rusty_smtp::outbound_queue::JobOutbound;
use rusty_store::fixtures::Fixtures;
use rusty_store::{BlobStore, FsBlobStore, Store, StoreConfig};
use rusty_svc::{
    certified_key, Accept, BoundListener, Context, DirectTcp, DirectTls, ListenerSpec, SystemClock, SessionHandler, StaticCertResolver, TlsProvider,
    TracingEvents,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "postgres")]
use rusty_store::fixtures::pg_db as pg;

pub const HOST: &str = "mx.example.net";

pub struct Backend {
    pub name: &'static str,
    pub store: Store,
    pub fx: Fixtures,
    pub dir: Directory,
    pub tmp: tempfile::TempDir,
    #[cfg(feature = "postgres")]
    _db: Option<pg::PgTestDb>,
}

impl Drop for Backend {
    fn drop(&mut self) {}
}

pub async fn sqlite_backend() -> Backend {
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", tmp.path().join("meta.sqlite").display());
    let store = Store::open(StoreConfig::sqlite(url)).await.unwrap();
    Backend {
        name: "sqlite",
        dir: Directory::with_repo(store.repos().directory.clone()),
        fx: Fixtures::of(&store),
        store,
        tmp,
        #[cfg(feature = "postgres")]
        _db: None,
    }
}

#[cfg(feature = "postgres")]
pub async fn pg_backend() -> Option<Backend> {
    let url = pg::pg_admin_url()?;
    let db = pg::PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    Some(Backend {
        name: "postgres",
        dir: Directory::with_repo(store.repos().directory.clone()),
        fx: Fixtures::admin_of(&db, &store).await,
        store,
        tmp: tempfile::tempdir().unwrap(),
        _db: Some(db),
    })
}

pub async fn backends() -> Vec<Backend> {
    #[allow(unused_mut)]
    let mut v = vec![sqlite_backend().await];
    #[cfg(feature = "postgres")]
    if let Some(b) = pg_backend().await {
        v.push(b);
    }
    v
}

pub async fn for_each_backend<F, Fut>(f: F)
where
    F: Fn(Backend) -> Fut,
    Fut: Future<Output = ()>,
{
    for b in backends().await {
        eprintln!("-- backend {}", b.name);
        f(b).await;
    }
}

impl Backend {
    pub async fn count(&self, table: &str) -> i64 {
        self.fx.scalar(&format!("SELECT count(*) FROM {table}"), &[]).await
    }

    /// Domain `example.net` with accounts alice and bob (and the aliases the ctl would create).
    pub async fn seed(&self) -> (AccountId, AccountId) {
        let dom = self.dir.create_domain("example.net").await.unwrap();
        let alice = self.dir.create_account("alice", dom, Some("pw-alice-1234"), None).await.unwrap();
        let bob = self.dir.create_account("bob", dom, Some("pw-bob-12345"), None).await.unwrap();
        self.dir.add_alias("postmaster", dom, alice).await.unwrap();
        (alice, bob)
    }

    /// What the Inbox of `acc` holds, raw bytes, oldest first.
    pub async fn inbox(&self, acc: AccountId, blobs: &Arc<dyn BlobStore>) -> Vec<Vec<u8>> {
        self.folder(acc, FolderId(1), blobs).await
    }

    pub async fn folder(&self, acc: AccountId, folder: FolderId, blobs: &Arc<dyn BlobStore>) -> Vec<Vec<u8>> {
        let mb = rusty_store::Mailbox::open_with(self.store.repos().mailbox.clone(), blobs.clone(), acc).await.unwrap();
        let mut out = Vec::new();
        let mut after = 0;
        loop {
            let page = mb.list_folder(folder, after, 100).await.unwrap();
            if page.is_empty() {
                return out;
            }
            for m in &page {
                let r = mb.get_raw_ref(m.id).await.unwrap();
                out.push(blobs.get(&r.blob).await.unwrap().to_vec());
                after = m.uid;
            }
        }
    }
}

pub struct Opts {
    pub options: InboundOptions,
    pub max_message_bytes: u64,
    pub max_recipients: u32,
    pub tls: bool,
    /// Serve submission (AUTH over TLS, queueing of remote recipients) instead of port 25.
    pub submission: bool,
    /// Accept TLS before the first byte (port 465) instead of plain TCP.
    pub implicit_tls: bool,
    pub per_hour: u32,
    pub auth_failures_max: u32,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts { options: InboundOptions::default(), max_message_bytes: 1 << 20, max_recipients: 100, tls: false, submission: false, implicit_tls: false, per_hour: 200, auth_failures_max: 10 }
    }
}

pub struct Server {
    pub addr: SocketAddr,
    pub token: CancellationToken,
    pub blobs: Arc<dyn BlobStore>,
    pub ctx: Context,
    pub cert_der: Vec<u8>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    pub async fn stop(self) {
        self.token.cancel();
        let _ = self.task.await;
    }
}

pub async fn start(b: &Backend, opts: Opts) -> Server {
    let blobs_root = b.tmp.path().join("blobs");
    FsBlobStore::init(&blobs_root).await.unwrap();
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::open(&blobs_root).await.unwrap());
    let mut cfg = StartupConfig::dev(b.tmp.path());
    cfg.hostname = Some(HOST.into());
    cfg.max_message_bytes = opts.max_message_bytes;
    cfg.smtp.max_recipients = opts.max_recipients;
    cfg.limits.auth_failures_max = opts.auth_failures_max;
    let (tls, cert_der) = if opts.tls {
        let ck = rcgen::generate_simple_self_signed(vec![HOST.to_string()]).unwrap();
        let cert_pem = ck.cert.pem();
        let key_pem = ck.signing_key.serialize_pem();
        let der = ck.cert.der().to_vec();
        let key = certified_key(cert_pem.as_bytes(), key_pem.as_bytes()).unwrap();
        (TlsProvider::from_resolver(Arc::new(StaticCertResolver::new(key))).unwrap(), der)
    } else {
        (TlsProvider::unavailable().unwrap(), Vec::new())
    };
    let ctx = Context::new(b.store.repos(), blobs.clone(), Arc::new(cfg), tls).await.unwrap();
    let handler: Arc<dyn SessionHandler> = if opts.submission {
        let deps = SubmissionDeps {
            outbound: Arc::new(JobOutbound::new(ctx.jobs.clone(), ctx.directory.clone())),
            limiter: Arc::new(MemorySubmissionLimiter::new(opts.per_hour, Arc::new(SystemClock::new()))),
        };
        Arc::new(InboundHandler::submission(&ctx, opts.options, deps))
    } else {
        Arc::new(InboundHandler::new(&ctx, opts.options))
    };
    let accept: Arc<dyn Accept> = if opts.implicit_tls { Arc::new(DirectTls::new(ctx.tls.clone())) } else { Arc::new(DirectTcp) };
    let listener = BoundListener::bind(ListenerSpec {
        name: "smtp-25",
        addr: "127.0.0.1:0".parse().unwrap(),
        max_connections: 100,
        max_per_ip: 100,
    })
    .await
    .unwrap();
    let addr = listener.local_addr();
    let token = CancellationToken::new();
    let t = token.clone();
    let task = tokio::spawn(async move {
        let _ = listener.serve(accept, handler, Arc::new(TracingEvents), t, Duration::from_secs(5)).await;
    });
    Server { addr, token, blobs, ctx, cert_der, task }
}

/// A line-oriented SMTP client over any stream.
pub struct Client {
    stream: BufReader<Box<dyn rusty_svc::AsyncStream>>,
}

impl Client {
    pub async fn connect(addr: SocketAddr) -> Client {
        let tcp = TcpStream::connect(addr).await.unwrap();
        Client { stream: BufReader::new(Box::new(tcp)) }
    }

    /// One full reply (all continuation lines), as raw text with CRLFs.
    pub async fn reply(&mut self) -> String {
        let mut all = String::new();
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(10), self.stream.read_line(&mut line))
                .await
                .expect("the server did not answer in time")
                .unwrap();
            if n == 0 {
                return all;
            }
            all.push_str(&line);
            if line.as_bytes().get(3) != Some(&b'-') {
                return all;
            }
        }
    }

    pub async fn send(&mut self, bytes: &[u8]) {
        self.stream.get_mut().write_all(bytes).await.unwrap();
        self.stream.get_mut().flush().await.unwrap();
    }

    /// Like `send`, but a closed connection is an answer, not a panic.
    pub async fn try_send(&mut self, bytes: &[u8]) -> bool {
        self.stream.get_mut().write_all(bytes).await.is_ok() && self.stream.get_mut().flush().await.is_ok()
    }

    pub async fn cmd(&mut self, line: &str) -> String {
        self.send(format!("{line}\r\n").as_bytes()).await;
        self.reply().await
    }

    /// Read until the peer closes; true if it did within `within`.
    pub async fn closed_within(&mut self, within: Duration) -> bool {
        let mut buf = [0u8; 256];
        tokio::time::timeout(within, async {
            loop {
                match self.stream.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        })
        .await
        .is_ok()
    }

    /// Greeting + EHLO.
    pub async fn hello(addr: SocketAddr) -> Client {
        let mut c = Client::connect(addr).await;
        let g = c.reply().await;
        assert!(g.starts_with("220 "), "{g}");
        let e = c.cmd("EHLO client.example.org").await;
        assert!(e.starts_with("250"), "{e}");
        c
    }

    /// MAIL, RCPT(s), DATA and the body; returns the reply to the final dot.
    pub async fn mail(&mut self, from: &str, rcpts: &[&str], body: &[u8]) -> String {
        let r = self.cmd(&format!("MAIL FROM:<{from}>")).await;
        assert!(r.starts_with("250"), "{r}");
        for rc in rcpts {
            let r = self.cmd(&format!("RCPT TO:<{rc}>")).await;
            assert!(r.starts_with("250"), "{r}");
        }
        let r = self.cmd("DATA").await;
        assert!(r.starts_with("354"), "{r}");
        self.send(body).await;
        self.send(b"\r\n.\r\n").await;
        self.reply().await
    }

    pub async fn upgrade_tls(self, cert_der: &[u8]) -> Client {
        use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
        use tokio_rustls::rustls::{ClientConfig, RootCertStore};
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(cert_der.to_vec())).unwrap();
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let cfg = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
        let name = ServerName::try_from(HOST.to_string()).unwrap();
        let inner = self.stream.into_inner();
        let tls = connector.connect(name, inner).await.expect("TLS handshake with the server");
        Client { stream: BufReader::new(Box::new(tls)) }
    }
}

/// Standard base64 (tests only).
pub fn b64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn plain(user: &str, password: &str) -> String {
    b64(format!("\0{user}\0{password}").as_bytes())
}

impl Client {
    /// Implicit TLS (port 465): the handshake comes before the greeting.
    pub async fn connect_tls(addr: SocketAddr, cert_der: &[u8]) -> Client {
        Client::connect(addr).await.upgrade_tls(cert_der).await
    }

    /// Greeting, STARTTLS, TLS, EHLO again: a submission session ready for AUTH.
    pub async fn secure_hello(addr: SocketAddr, cert_der: &[u8]) -> Client {
        let mut c = Client::connect(addr).await;
        assert!(c.reply().await.starts_with("220 "));
        let e = c.cmd("EHLO client.example.org").await;
        assert!(e.contains("STARTTLS"), "{e}");
        assert!(c.cmd("STARTTLS").await.starts_with("220 "));
        let mut c = c.upgrade_tls(cert_der).await;
        let e = c.cmd("EHLO client.example.org").await;
        assert!(e.contains("AUTH PLAIN LOGIN"), "{e}");
        c
    }
}

/// Every backend with short waits (300 ms) and a two-connection pool, for the saturation tests.
pub async fn backends_short_waits() -> Vec<Backend> {
    let ms = Duration::from_millis(300);
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", tmp.path().join("meta.sqlite").display());
    let store = Store::open(StoreConfig::Sqlite { url, busy_timeout: ms }).await.unwrap();
    #[allow(unused_mut)]
    let mut v = vec![Backend {
        name: "sqlite",
        dir: Directory::with_repo(store.repos().directory.clone()),
        fx: Fixtures::of(&store),
        store,
        tmp,
        #[cfg(feature = "postgres")]
        _db: None,
    }];
    #[cfg(feature = "postgres")]
    if let Some(url) = pg::pg_admin_url() {
        let db = pg::PgTestDb::create(&url).await;
        // A statement waiting on a lock is cut at the role/database timeout (the role-level setting of production).
        db.set_database_setting("statement_timeout", "300ms").await;
        let limits = rusty_store::PgLimits {
            max_connections: 2,
            acquire_timeout: ms,
            lock_timeout: ms,
            queue_timeout: ms,
            ..rusty_store::PgLimits::default()
        };
        let cfg = StoreConfig::Postgres { url: db.url().to_owned(), pool: limits, security: rusty_store::PgSecurity::default() };
        let store = Store::open(cfg).await.unwrap();
        v.push(Backend {
            name: "postgres",
            dir: Directory::with_repo(store.repos().directory.clone()),
            fx: Fixtures::admin_of(&db, &store).await,
            store,
            tmp: tempfile::tempdir().unwrap(),
            _db: Some(db),
        });
    }
    v
}

pub async fn for_each_backend_short_waits<F, Fut>(f: F)
where
    F: Fn(Backend) -> Fut,
    Fut: Future<Output = ()>,
{
    for b in backends_short_waits().await {
        eprintln!("-- backend {}", b.name);
        f(b).await;
    }
}
