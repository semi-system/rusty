#![allow(dead_code)]
//! Harness of the IMAP session suites: a backend (SQLite always, Postgres when `RUSTY_TEST_PG_URL`
//! is set and the `postgres` feature is on), a `Context` built the composition-root way, and a
//! client connection that talks to a real `ImapHandler` over an in-memory pipe.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rusty_core::config::StartupConfig;
use rusty_core::AccountId;
use rusty_directory::Directory;
use rusty_imap::{ImapHandler, Limits};
use rusty_store::{BlobStore, FsBlobStore, Mailbox, Store, StoreConfig};
use rusty_svc::{BoxedStream, Context, FileSecretSource, PeerInfo, SessionHandler, TlsProvider};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "postgres")]
pub use rusty_store::fixtures::pg_db as pg;

pub struct Backend {
    pub name: &'static str,
    pub store: Store,
    pub dir: Directory,
    pub blobs: Arc<dyn BlobStore>,
    pub tmp: tempfile::TempDir,
    #[cfg(feature = "postgres")]
    _db: Option<pg::PgTestDb>,
}

impl Drop for Backend {
    fn drop(&mut self) {}
}

async fn finish(name: &'static str, store: Store, tmp: tempfile::TempDir, #[cfg(feature = "postgres")] db: Option<pg::PgTestDb>) -> Backend {
    let root = tmp.path().join("blobs");
    FsBlobStore::init(&root).await.unwrap();
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::open(&root).await.unwrap());
    Backend {
        name,
        dir: Directory::with_repo(store.repos().directory.clone()),
        store,
        blobs,
        tmp,
        #[cfg(feature = "postgres")]
        _db: db,
    }
}

pub async fn sqlite_backend() -> Backend {
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", tmp.path().join("meta.sqlite").display());
    let store = Store::open(StoreConfig::sqlite(url)).await.unwrap();
    finish(
        "sqlite",
        store,
        tmp,
        #[cfg(feature = "postgres")]
        None,
    )
    .await
}

#[cfg(feature = "postgres")]
pub async fn pg_backend() -> Option<Backend> {
    let url = pg::pg_admin_url()?;
    let db = pg::PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    Some(finish("postgres", store, tempfile::tempdir().unwrap(), Some(db)).await)
}

pub async fn backends() -> Vec<Backend> {
    vec![pg_backend().await.expect("a Postgres test server (RUSTY_TEST_PG_URL)")]
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

pub const HOST: &str = "mail.test";

impl Backend {
    /// A `Context` with the dev TLS certificate (STARTTLS is offered).
    pub async fn ctx(&self) -> Context {
        self.ctx_with(|_| {}).await
    }

    pub async fn ctx_with(&self, tweak: impl FnOnce(&mut StartupConfig)) -> Context {
        let mut cfg = StartupConfig::dev(self.tmp.path());
        cfg.hostname = Some(HOST.into());
        tweak(&mut cfg);
        let tls = TlsProvider::from_config(&cfg.tls, HOST, Arc::new(FileSecretSource::new())).unwrap();
        Context::new(self.store.repos(), self.blobs.clone(), Arc::new(cfg), tls).await.unwrap()
    }

    /// A `Context` without any TLS (no STARTTLS offered).
    pub async fn ctx_without_tls(&self) -> Context {
        let mut cfg = StartupConfig::dev(self.tmp.path());
        cfg.hostname = Some(HOST.into());
        cfg.tls.dev_self_signed = false;
        Context::new(self.store.repos(), self.blobs.clone(), Arc::new(cfg), TlsProvider::unavailable().unwrap()).await.unwrap()
    }

    /// Create the domain of `addr` and the account in it, with `password`.
    pub async fn account(&self, addr: &str, password: &str) -> AccountId {
        let (local, domain) = addr.split_once('@').unwrap();
        let dom = self.dir.create_domain(domain).await.unwrap();
        self.account_in(dom, local, password).await
    }

    pub async fn account_in(&self, dom: rusty_core::DomainId, local: &str, password: &str) -> AccountId {
        let acc = self.dir.create_account(local, dom, None, None).await.unwrap();
        self.dir.set_password(acc, password).await.unwrap();
        acc
    }

    /// The account's mailbox through a hub of `ctx` (what the server itself uses).
    pub async fn mailbox(&self, ctx: &Context, acc: AccountId) -> Mailbox {
        ctx.hub.open_for(acc).await.unwrap()
    }
}

pub trait Rw: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Rw for T {}

const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub fn peer(tls: bool) -> PeerInfo {
    PeerInfo { addr: SocketAddr::from(([192, 0, 2, 7], 40000)), tls }
}

/// A client connected to a running session task.
pub struct Conn {
    pub io: Box<dyn Rw>,
    buf: Vec<u8>,
    tag: u32,
    pub shutdown: CancellationToken,
    pub task: tokio::task::JoinHandle<()>,
}

impl Conn {
    /// Serve a connection with `handler`; `tls` is whether the transport counts as protected.
    pub fn open(handler: Arc<ImapHandler>, tls: bool) -> Conn {
        Conn::open_peer(handler, peer(tls))
    }

    pub fn open_peer(handler: Arc<ImapHandler>, peer: PeerInfo) -> Conn {
        let (client, server) = tokio::io::duplex(1 << 20);
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let task = tokio::spawn(async move {
            let stream: BoxedStream = Box::new(server);
            handler.handle(stream, peer, token).await;
        });
        Conn { io: Box::new(client), buf: Vec::new(), tag: 0, shutdown, task }
    }

    pub async fn send(&mut self, s: &str) {
        self.send_bytes(s.as_bytes()).await;
    }

    /// Like `send_bytes`, but a closed peer is not a failure (returns whether everything was written).
    pub async fn try_send_bytes(&mut self, b: &[u8]) -> bool {
        matches!(tokio::time::timeout(IO_TIMEOUT, self.io.write_all(b)).await, Ok(Ok(())))
    }

    pub async fn send_bytes(&mut self, b: &[u8]) {
        tokio::time::timeout(IO_TIMEOUT, self.io.write_all(b)).await.expect("write timed out").expect("write");
        let _ = self.io.flush().await;
    }

    async fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 8192];
        match tokio::time::timeout(IO_TIMEOUT, self.io.read(&mut chunk)).await {
            Err(_) => panic!("timed out waiting for the server; buffer so far: {:?}", String::from_utf8_lossy(&self.buf)),
            Ok(Ok(0)) | Ok(Err(_)) => false,
            Ok(Ok(n)) => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
        }
    }

    /// One response line without CRLF; a literal inside it is kept inline as `{n}\r\n<data>`.
    /// `None` when the server closed the connection.
    pub async fn try_line(&mut self) -> Option<String> {
        let mut out: Vec<u8> = Vec::new();
        loop {
            // A complete line in the buffer?
            if let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=i).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                out.extend_from_slice(&line);
                // Does it end with a literal announcement?
                if let Some(n) = literal_len(&line) {
                    while self.buf.len() < n {
                        if !self.fill().await {
                            return None;
                        }
                    }
                    let data: Vec<u8> = self.buf.drain(..n).collect();
                    out.extend_from_slice(b"\r\n");
                    out.extend_from_slice(&data);
                    continue;
                }
                return Some(String::from_utf8_lossy(&out).into_owned());
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    pub async fn line(&mut self) -> String {
        self.try_line().await.expect("connection closed while waiting for a line")
    }

    /// Run `cmd` with a fresh tag; returns every line up to and including the tagged status.
    pub async fn cmd(&mut self, cmd: &str) -> Vec<String> {
        self.tag += 1;
        let tag = format!("t{}", self.tag);
        self.send(&format!("{tag} {cmd}\r\n")).await;
        self.until(&tag).await
    }

    pub async fn until(&mut self, tag: &str) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            let l = self.line().await;
            let done = l.starts_with(&format!("{tag} "));
            lines.push(l);
            if done {
                return lines;
            }
        }
    }

    /// `cmd`, which must complete OK; the untagged lines before the status.
    pub async fn ok(&mut self, cmd: &str) -> Vec<String> {
        let mut lines = self.cmd(cmd).await;
        let status = lines.pop().unwrap();
        assert!(status.contains(" OK"), "{cmd}: {status}");
        lines
    }

    /// `cmd` and its tagged status line only (untagged data dropped).
    pub async fn status(&mut self, cmd: &str) -> String {
        self.cmd(cmd).await.pop().unwrap()
    }

    pub async fn login(&mut self, user: &str, pw: &str) {
        let lines = self.cmd(&format!("LOGIN \"{user}\" \"{pw}\"")).await;
        assert!(lines.last().unwrap().contains(" OK"), "{lines:?}");
    }

    /// True when the server has closed the connection (after draining what it still sent).
    pub async fn closed(&mut self) -> bool {
        loop {
            if self.try_line().await.is_none() {
                return true;
            }
        }
    }
}

fn literal_len(line: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(line).ok()?;
    let inner = s.strip_suffix('}')?;
    let open = inner.rfind('{')?;
    inner.get(open + 1..)?.parse().ok()
}

/// A simple message for the account's inbox.
pub fn message(subject: &str, body: &str) -> String {
    format!("From: Sender <sender@example.org>\r\nTo: rcpt@mail.test\r\nSubject: {subject}\r\nMessage-ID: <{subject}@test>\r\nDate: Fri, 06 Nov 2026 10:00:00 +0000\r\n\r\n{body}\r\n")
}

pub async fn deliver(mb: &Mailbox, folder: i64, subject: &str, body: &str) -> rusty_store::MessageRef {
    mb.append(rusty_core::FolderId(folder), bytes::Bytes::from(message(subject, body)), rusty_core::Flags::empty()).await.unwrap()
}

pub fn handler(ctx: Context) -> Arc<ImapHandler> {
    Arc::new(ImapHandler::new(ctx))
}

pub fn handler_with(ctx: Context, limits: Limits) -> Arc<ImapHandler> {
    Arc::new(ImapHandler::new(ctx).with_limits(limits))
}

/// Every backend with short waits (300 ms) and a two-connection pool, for the saturation tests.
pub async fn for_each_backend_short_waits<F, Fut>(f: F)
where
    F: Fn(Backend, rusty_store::fixtures::Fixtures) -> Fut,
    Fut: Future<Output = ()>,
{
    let ms = Duration::from_millis(300);
    #[cfg(feature = "postgres")]
    if let Some(url) = pg::pg_admin_url() {
        let db = pg::PgTestDb::create(&url).await;
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
        let fx = rusty_store::fixtures::Fixtures::admin_of(&db, &store).await;
        eprintln!("-- backend postgres");
        f(finish("postgres", store, tempfile::tempdir().unwrap(), Some(db)).await, fx).await;
    }
}
